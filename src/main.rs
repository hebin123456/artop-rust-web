//! artop-rust-web —— Git-like 协同 ARXML 模型仓库（纯 Rust 全栈 POC）
//!
//! 一个进程里同时提供：
//!   - 账号 / 仓库级 RBAC（reader<developer<reviewer<maintainer<owner）
//!   - Git-like 版本库：内容寻址 blob + 提交 DAG + 分支指针 CAS + 三方合并
//!   - 评审工作流：开 MR -> 他人审批 -> maintainer 合入
//!   - 实时协作：WebSocket + Redis pub/sub 扇出
//!   - 静态前端：static/index.html 作为端到端验证页
//!
//! 回答最初的问题："协同 + 账号权限是不是必须上 Spring？"
//! 结论：不需要。axum + sqlx + Redis 全部用 Rust 一套写完，形态与 Spring 等价。

mod api;
mod auth;
mod config;
mod db;
mod error;
mod rbac;
mod realtime;
mod review;
mod state;
mod vcs;

use std::sync::Arc;
use std::time::Duration;

use redis::aio::ConnectionManager;
use sqlx::mysql::MySqlPoolOptions;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::config::Config;
use crate::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,sqlx=warn".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cfg = Arc::new(Config::from_env());

    // MySQL：连接池 + 引导 schema
    let pool = MySqlPoolOptions::new()
        .max_connections(16)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&cfg.database_url)
        .await?;
    db::init_schema(&pool).await?;
    tracing::info!("schema 就绪");

    // Redis：连接管理器（克隆安全，自带重连），用于锁/缓存/发布订阅
    let redis_client = redis::Client::open(cfg.redis_url.as_str())?;
    let redis = ConnectionManager::new(redis_client.clone())
        .await
        .map_err(|e| anyhow::anyhow!("redis 连接失败：{e}"))?;

    let state = AppState { db: pool, redis, redis_client, cfg: cfg.clone() };

    let app = api::router(state)
        .fallback_service(ServeDir::new("static"))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    tracing::info!("artop-rust-web 监听 http://{}", cfg.bind);
    axum::serve(listener, app).await?;
    Ok(())
}