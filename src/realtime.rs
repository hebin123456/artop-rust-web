//! 实时协作（Redis pub/sub 扇出 + WebSocket 下发）
//!
//! 每个在线编辑器打开一条 /ws?repo_id=&token= 长连接：
//!   - 服务端订阅 Redis 频道 repo:{id}:events，把事件推给所有在线端；
//!   - 客户端发来的协作编辑指令，加上 origin/by 后 PUBLISH 回去，其他人即时收到。
//! 用 origin(连接 id) 去重，避免自己的消息被回环推给自己。
//! 这样多实例部署时也天然互通：Redis 是唯一扇出总线。

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;

use crate::auth::{self, AuthUser};
use crate::error::AppResult;
use crate::rbac;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct WsQuery {
    pub repo_id: i64,
    pub token: String,
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(q): Query<WsQuery>,
    State(state): State<AppState>,
) -> AppResult<Response> {
    // WebSocket 无法带自定义头，token 从 query 取；先鉴权再升级
    let user = auth::user_from_token(&state, &q.token)?;
    rbac::require(&state.db, q.repo_id, user.id, rbac::P_READ).await?;
    Ok(ws.on_upgrade(move |sock| handle(state, sock, q.repo_id, user)))
}

async fn handle(state: AppState, sock: WebSocket, repo_id: i64, user: AuthUser) {
    let channel = state.channel_repo(repo_id);
    let conn_id = uuid::Uuid::new_v4().to_string();

    // 1) 先建 Redis 订阅，再发 hello：客户端收到 hello 即代表"已订阅、不会漏事件"
    let pubsub_conn = match state.redis_client.get_async_connection().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("WS redis pubsub 连接失败: {e}");
            return;
        }
    };
    let mut pubsub = pubsub_conn.into_pubsub();
    if let Err(e) = pubsub.subscribe(&channel).await {
        tracing::warn!("WS 订阅 {channel} 失败: {e}");
        return;
    }

    let (mut sink, mut stream) = sock.split();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<String>(256);

    // 2) Redis -> 本连接。断线后 out_tx 发送失败即自动退出。
    {
        let sub_conn = conn_id.clone();
        tokio::spawn(async move {
            let mut msgs = pubsub.on_message();
            while let Some(msg) = msgs.next().await {
                let payload: String = match msg.get_payload() {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                // 跳过自己发出的，防止回环
                if serde_json::from_str::<serde_json::Value>(&payload)
                    .ok()
                    .and_then(|v| v.get("origin").and_then(|x| x.as_str()).map(|s| s == sub_conn))
                    .unwrap_or(false)
                {
                    continue;
                }
                if out_tx.send(payload).await.is_err() {
                    break;
                }
            }
        });
    }

    // 3) 就绪握手
    if sink
        .send(Message::Text(
            serde_json::json!({
                "type": "hello",
                "conn_id": conn_id,
                "repo_id": repo_id,
                "user": user.username,
            })
            .to_string(),
        ))
        .await
        .is_err()
    {
        return;
    }

    // 4) 下游：channel -> WebSocket
    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if sink.send(Message::Text(m)).await.is_err() {
                break;
            }
        }
    });

    // 5) 上游：WebSocket -> Redis，转发协作编辑指令
    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Text(t) => {
                let payload = match serde_json::from_str::<serde_json::Value>(&t) {
                    Ok(mut v) => {
                        v["origin"] = serde_json::json!(conn_id);
                        if v.get("by").is_none() {
                            v["by"] = serde_json::json!(user.username);
                        }
                        v.to_string()
                    }
                    Err(_) => continue,
                };
                let _: Result<(), _> = redis::cmd("PUBLISH")
                    .arg(&channel)
                    .arg(payload)
                    .query_async(&mut state.redis.clone())
                    .await;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    writer.abort();
}