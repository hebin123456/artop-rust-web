use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use sqlx::{MySqlPool, Row};

use crate::error::{AppError, AppResult};
use crate::state::AppState;

// ============ 报文类型 ============

#[derive(Debug, Deserialize)]
pub struct RefOut {
    pub feat: String,
    pub tgt: String,
}

#[derive(Debug, Deserialize)]
pub struct ChangeItem {
    pub uid: String,
    pub path: String,
    pub cls: String,
    /// A=新增 M=修改 D=删除
    pub op: String,
    #[serde(default)]
    pub sn: Option<String>,
    #[serde(default)]
    pub attrs: serde_json::Value,
    #[serde(default)]
    pub refs_out: Vec<RefOut>,
}

#[derive(Debug, Deserialize)]
pub struct PushReq {
    pub branch: String,
    /// 客户端手里的分支头；新建分支时为空
    #[serde(default)]
    pub base_commit: Option<String>,
    pub message: String,
    pub changes: Vec<ChangeItem>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ChangeRow {
    pub element_uid: String,
    pub path: String,
    pub op: String,
    pub old_blob: Option<String>,
    pub new_blob: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PushResp {
    pub commit_id: String,
    pub branch: String,
    pub parent: Option<String>,
    pub changes: usize,
    pub applied: String,
}

// ============ 基础工具 ============

pub fn sha1_hex(s: &str) -> String {
    let mut h = Sha1::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

/// 元素的规范化序列化体 -> 内容哈希（内容寻址，天然去重）
fn blob_of(item: &ChangeItem) -> (String, serde_json::Value) {
    let body = serde_json::json!({
        "cls": item.cls,
        "sn": item.sn,
        "path": item.path,
        "attrs": item.attrs,
    });
    let canon = serde_json::to_string(&body).unwrap_or_default();
    (sha1_hex(&canon), body)
}

pub async fn head_of(db: &MySqlPool, repo_id: i64, branch: &str) -> AppResult<Option<String>> {
    let r: Option<String> =
        sqlx::query_scalar("SELECT commit_id FROM ref WHERE repo_id=? AND name=?")
            .bind(repo_id)
            .bind(branch)
            .fetch_optional(db)
            .await?;
    Ok(r)
}

// ============ 提交 / 推送 ============

/// push = commit + 推进分支指针。整笔在一个事务里，要么全成要么全败。
pub async fn push(
    state: &AppState,
    repo_id: i64,
    author_id: i64,
    req: PushReq,
) -> AppResult<PushResp> {
    if req.changes.is_empty() {
        return Err(AppError::BadRequest("changes 不能为空".into()));
    }

    // 1) Redis 分布式锁：同一分支的推送串行化，冲突早失败
    let lock_key = state.key_push_lock(repo_id, &req.branch);
    let lock_token = uuid::Uuid::new_v4().to_string();
    let got: Option<String> = redis::cmd("SET")
        .arg(&lock_key)
        .arg(&lock_token)
        .arg("NX")
        .arg("PX")
        .arg(3000)
        .query_async(&mut state.redis.clone())
        .await?;
    if got.is_none() {
        return Err(AppError::Conflict("该分支正有其他推送进行中，请稍后重试".into()));
    }

    let result = push_txn(state, repo_id, author_id, &req).await;

    // 释放锁（PX 保证进程崩溃后也会自动过期）
    let _: Result<(), _> = redis::cmd("DEL")
        .arg(&lock_key)
        .query_async(&mut state.redis.clone())
        .await;

    let resp = result?;

    // 2) 写穿分支头缓存
    let _: Result<(), _> = redis::cmd("SET")
        .arg(state.key_ref(repo_id, &req.branch))
        .arg(&resp.commit_id)
        .query_async(&mut state.redis.clone())
        .await;

    // 3) 广播协作事件（同仓库其他在线编辑器实时收到）
    let evt = serde_json::json!({
        "type": "push",
        "repo_id": repo_id,
        "branch": req.branch,
        "commit": resp.commit_id,
        "author_id": author_id,
        "changes": resp.changes,
        "applied": resp.applied,
    })
    .to_string();
    let _: Result<(), _> = redis::cmd("PUBLISH")
        .arg(state.channel_repo(repo_id))
        .arg(evt)
        .query_async(&mut state.redis.clone())
        .await;

    Ok(resp)
}

async fn push_txn(
    state: &AppState,
    repo_id: i64,
    author_id: i64,
    req: &PushReq,
) -> AppResult<PushResp> {
    let mut tx = state.db.begin().await?;

    // 行锁：已存在分支的并发推送在此串行化
    let current: Option<String> =
        sqlx::query_scalar("SELECT commit_id FROM ref WHERE repo_id=? AND name=? FOR UPDATE")
            .bind(repo_id)
            .bind(&req.branch)
            .fetch_optional(&mut *tx)
            .await?;

    // 快进检查
    match (&current, &req.base_commit) {
        (Some(h), Some(b)) if h == b => {}
        (None, None) => {}
        (Some(h), _) => {
            return Err(AppError::Conflict(format!(
                "非快进推送：分支 {} 的 HEAD 已是 {}，你基于 {}，请先拉取",
                req.branch,
                h,
                req.base_commit.clone().unwrap_or_else(|| "(空)".into())
            )))
        }
        (None, Some(b)) => {
            return Err(AppError::Conflict(format!(
                "分支 {} 不存在，但 base_commit 声称是 {b}",
                req.branch
            )))
        }
    }

    // 生成提交 id：内容寻址（分支+父+作者+消息+变更指纹）
    let mut fp = Sha1::new();
    for c in &req.changes {
        fp.update(c.uid.as_bytes());
        fp.update(b"|");
        fp.update(c.op.as_bytes());
        fp.update(b"|");
        fp.update(blob_of(c).0.as_bytes());
    }
    let commit_id = sha1_hex(&format!(
        "{}|{}|{}|{}|{}|{:x}",
        repo_id,
        req.branch,
        req.base_commit.clone().unwrap_or_default(),
        req.message,
        author_id,
        fp.finalize()
    ));

    let mut seq: u32 = 0;
    for c in &req.changes {
        let op = c.op.to_uppercase();
        let (blob_id, body) = blob_of(c);

        // 记录旧 blob，供 diff/回滚/审计使用
        let prev: Option<String> =
            sqlx::query_scalar("SELECT blob_id FROM element WHERE repo_id=? AND element_uid=?")
                .bind(repo_id)
                .bind(&c.uid)
                .fetch_optional(&mut *tx)
                .await?;

        match op.as_str() {
            "A" | "M" => {
                sqlx::query(
                    "INSERT IGNORE INTO content_blob(repo_id,blob_id,cls,sn,body) VALUES (?,?,?,?,?)",
                )
                .bind(repo_id)
                .bind(&blob_id)
                .bind(&c.cls)
                .bind(c.sn.clone().unwrap_or_default())
                .bind(&body)
                .execute(&mut *tx)
                .await?;

                sqlx::query(
                    "INSERT INTO element(repo_id,element_uid,path,cls,blob_id) VALUES (?,?,?,?,?) \
                     ON DUPLICATE KEY UPDATE path=VALUES(path), cls=VALUES(cls), blob_id=VALUES(blob_id)",
                )
                .bind(repo_id)
                .bind(&c.uid)
                .bind(&c.path)
                .bind(&c.cls)
                .bind(&blob_id)
                .execute(&mut *tx)
                .await?;

                // 该元素的出边整体重写，保证引用图与最新内容一致
                sqlx::query("DELETE FROM ref_edge WHERE repo_id=? AND src_uid=?")
                    .bind(repo_id)
                    .bind(&c.uid)
                    .execute(&mut *tx)
                    .await?;
                for r in &c.refs_out {
                    sqlx::query("INSERT INTO ref_edge(repo_id,src_uid,feat,tgt_uid) VALUES (?,?,?,?)")
                        .bind(repo_id)
                        .bind(&c.uid)
                        .bind(&r.feat)
                        .bind(&r.tgt)
                        .execute(&mut *tx)
                        .await?;
                }
            }
            "D" => {
                sqlx::query("DELETE FROM element WHERE repo_id=? AND element_uid=?")
                    .bind(repo_id)
                    .bind(&c.uid)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("DELETE FROM ref_edge WHERE repo_id=? AND (src_uid=? OR tgt_uid=?)")
                    .bind(repo_id)
                    .bind(&c.uid)
                    .bind(&c.uid)
                    .execute(&mut *tx)
                    .await?;
            }
            other => return Err(AppError::BadRequest(format!("未知 op: {other}（应为 A/M/D）"))),
        }

        let new_blob = if op == "D" { None } else { Some(blob_id.clone()) };
        sqlx::query(
            "INSERT INTO change_set(repo_id,commit_id,seq,element_uid,path,op,old_blob,new_blob) \
             VALUES (?,?,?,?,?,?,?,?)",
        )
        .bind(repo_id)
        .bind(&commit_id)
        .bind(seq)
        .bind(&c.uid)
        .bind(&c.path)
        .bind(&op)
        .bind(&prev)
        .bind(&new_blob)
        .execute(&mut *tx)
        .await?;

        seq += 1;
    }

    sqlx::query(
        "INSERT INTO commit_node(repo_id,commit_id,parent_id,parent2_id,author_id,msg) \
         VALUES (?,?,?,NULL,?,?)",
    )
    .bind(repo_id)
    .bind(&commit_id)
    .bind(&req.base_commit)
    .bind(author_id)
    .bind(&req.message)
    .execute(&mut *tx)
    .await?;

    // CAS 推进分支指针：这是"要么全成要么全败"的最后一道闸
    let applied = match &current {
        None => {
            sqlx::query("INSERT INTO ref(repo_id,name,kind,commit_id) VALUES (?,?,'branch',?)")
                .bind(repo_id)
                .bind(&req.branch)
                .bind(&commit_id)
                .execute(&mut *tx)
                .await?;
            "branch-created".to_string()
        }
        Some(h) => {
            let r = sqlx::query(
                "UPDATE ref SET commit_id=?, updated_at=NOW(3) \
                 WHERE repo_id=? AND name=? AND commit_id=?",
            )
            .bind(&commit_id)
            .bind(repo_id)
            .bind(&req.branch)
            .bind(h)
            .execute(&mut *tx)
            .await?;
            if r.rows_affected() != 1 {
                return Err(AppError::Conflict("非快进推送：分支在事务期间被他人推进".into()));
            }
            "fast-forward".to_string()
        }
    };

    sqlx::query("INSERT INTO audit_log(repo_id,user_id,action,detail) VALUES (?,?,?,?)")
        .bind(repo_id)
        .bind(author_id)
        .bind("push")
        .bind(format!(
            "{} -> {} ({})",
            req.branch,
            &commit_id[..8],
            req.message
        ))
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok(PushResp {
        commit_id,
        branch: req.branch.clone(),
        parent: req.base_commit.clone(),
        changes: req.changes.len(),
        applied,
    })
}

// ============ 分支 ============

pub async fn create_branch(
    state: &AppState,
    repo_id: i64,
    user_id: i64,
    name: &str,
    from: &str,
) -> AppResult<String> {
    let src = head_of(&state.db, repo_id, from)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("源分支 {from} 不存在")))?;

    let exists: Option<String> =
        sqlx::query_scalar("SELECT commit_id FROM ref WHERE repo_id=? AND name=?")
            .bind(repo_id)
            .bind(name)
            .fetch_optional(&state.db)
            .await?;
    if exists.is_some() {
        return Err(AppError::Conflict(format!("分支 {name} 已存在")));
    }

    sqlx::query("INSERT INTO ref(repo_id,name,kind,commit_id) VALUES (?,?,'branch',?)")
        .bind(repo_id)
        .bind(name)
        .bind(&src)
        .execute(&state.db)
        .await?;

    sqlx::query("INSERT INTO audit_log(repo_id,user_id,action,detail) VALUES (?,?,?,?)")
        .bind(repo_id)
        .bind(user_id)
        .bind("branch.create")
        .bind(format!("{name} <- {from}"))
        .execute(&state.db)
        .await?;

    Ok(src)
}

// ============ 提交链 / 变更推导 ============

/// 从 head 沿 parent 回溯到 stop_at（不含），返回由新到旧的提交序列
pub async fn change_path(
    db: &MySqlPool,
    repo_id: i64,
    head: &str,
    stop_at: Option<&str>,
) -> AppResult<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = Some(head.to_string());
    let mut guard = 0usize;
    while let Some(c) = cur {
        if Some(c.as_str()) == stop_at {
            break;
        }
        out.push(c.clone());
        let parent: Option<Option<String>> =
            sqlx::query_scalar("SELECT parent_id FROM commit_node WHERE repo_id=? AND commit_id=?")
                .bind(repo_id)
                .bind(&c)
                .fetch_optional(db)
                .await?;
        cur = parent.flatten();
        guard += 1;
        if guard > 100_000 {
            break;
        }
    }
    Ok(out)
}

async fn change_set_of(db: &MySqlPool, repo_id: i64, commit: &str) -> AppResult<Vec<ChangeRow>> {
    let rows = sqlx::query_as::<_, ChangeRow>(
        "SELECT element_uid, path, op, old_blob, new_blob FROM change_set \
         WHERE repo_id=? AND commit_id=? ORDER BY seq",
    )
    .bind(repo_id)
    .bind(commit)
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// 两个提交之间的净变更（后发生的覆盖先发生的）
pub async fn net_changes(
    db: &MySqlPool,
    repo_id: i64,
    head: &str,
    stop_at: Option<&str>,
) -> AppResult<Vec<ChangeRow>> {
    let chain = change_path(db, repo_id, head, stop_at).await?;
    let mut order: Vec<String> = Vec::new();
    let mut map: HashMap<String, ChangeRow> = HashMap::new();
    for cid in chain.iter().rev() {
        // 由旧到新，新的覆盖旧的
        for row in change_set_of(db, repo_id, cid).await? {
            if !map.contains_key(&row.element_uid) {
                order.push(row.element_uid.clone());
            }
            map.insert(row.element_uid.clone(), row);
        }
    }
    Ok(order
        .into_iter()
        .filter_map(|k| map.remove(&k))
        .collect())
}

pub async fn diff(
    state: &AppState,
    repo_id: i64,
    from: &str,
    to: &str,
) -> AppResult<serde_json::Value> {
    let parent: Option<Option<String>> =
        sqlx::query_scalar("SELECT parent_id FROM commit_node WHERE repo_id=? AND commit_id=?")
            .bind(repo_id)
            .bind(to)
            .fetch_optional(&state.db)
            .await?;
    let parent = parent.flatten();

    let rows = if parent.as_deref() == Some(from) {
        change_set_of(&state.db, repo_id, to).await?
    } else {
        // 非线性：union 到 from 为止的整条链
        net_changes(&state.db, repo_id, to, Some(from)).await?
    };

    let mut add = 0;
    let mut modify = 0;
    let mut del = 0;
    for r in &rows {
        match r.op.as_str() {
            "A" => add += 1,
            "M" => modify += 1,
            "D" => del += 1,
            _ => {}
        }
    }

    Ok(serde_json::json!({
        "from": from,
        "to": to,
        "linear": parent.as_deref() == Some(from),
        "summary": { "added": add, "modified": modify, "deleted": del },
        "changes": rows,
    }))
}

// ============ 影响分析（引用图反向遍历） ============

pub async fn impact(
    state: &AppState,
    repo_id: i64,
    uid: &str,
    depth: i64,
) -> AppResult<serde_json::Value> {
    let depth = depth.clamp(1, 20);
    let rows = sqlx::query(
        "WITH RECURSIVE aff(uid, d) AS ( \
           SELECT ? AS uid, 0 AS d \
           UNION \
           SELECT e.src_uid, a.d + 1 FROM ref_edge e JOIN aff a ON e.tgt_uid = a.uid \
           WHERE a.d < ? AND e.repo_id = ? \
         ) \
         SELECT uid, MIN(d) AS d FROM aff GROUP BY uid ORDER BY d, uid",
    )
    .bind(uid)
    .bind(depth)
    .bind(repo_id)
    .fetch_all(&state.db)
    .await?;

    let affected: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "uid": r.get::<String, _>("uid"),
                "depth": r.get::<i64, _>("d"),
            })
        })
        .collect();

    Ok(serde_json::json!({
        "root": uid,
        "max_depth": depth,
        "total": affected.len(),
        "affected": affected,
    }))
}

// ============ 祖先 / 合并基点 ============

pub async fn ancestors(db: &MySqlPool, repo_id: i64, head: &str) -> AppResult<HashSet<String>> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut q: VecDeque<String> = VecDeque::new();
    q.push_back(head.to_string());
    let mut guard = 0usize;
    while let Some(c) = q.pop_front() {
        if !seen.insert(c.clone()) {
            continue;
        }
        let row = sqlx::query(
            "SELECT parent_id, parent2_id FROM commit_node WHERE repo_id=? AND commit_id=?",
        )
        .bind(repo_id)
        .bind(&c)
        .fetch_optional(db)
        .await?;
        if let Some(row) = row {
            for p in [row.get::<Option<String>, _>("parent_id"), row.get::<Option<String>, _>("parent2_id")] {
                if let Some(p) = p {
                    q.push_back(p);
                }
            }
        }
        guard += 1;
        if guard > 200_000 {
            break;
        }
    }
    Ok(seen)
}

/// 从 b 侧向上走，第一个落在 a 祖先集合里的提交即合并基点
pub async fn merge_base(
    db: &MySqlPool,
    repo_id: i64,
    a: &str,
    b: &str,
) -> AppResult<Option<String>> {
    let anc_a = ancestors(db, repo_id, a).await?;
    let mut cur = Some(b.to_string());
    let mut guard = 0usize;
    while let Some(c) = cur {
        if anc_a.contains(&c) {
            return Ok(Some(c));
        }
        let parent: Option<Option<String>> =
            sqlx::query_scalar("SELECT parent_id FROM commit_node WHERE repo_id=? AND commit_id=?")
                .bind(repo_id)
                .bind(&c)
                .fetch_optional(db)
                .await?;
        cur = parent.flatten();
        guard += 1;
        if guard > 100_000 {
            break;
        }
    }
    Ok(None)
}

// ============ 三方合并（用于评审合入） ============

#[derive(Debug, Serialize)]
pub struct MergeReport {
    pub merge_commit: String,
    pub target_branch: String,
    pub source_branch: String,
    pub base: Option<String>,
    pub applied_changes: usize,
    pub conflicts: Vec<String>,
}

/// 预演：只算冲突，不落库（评审页可先看能不能合）
pub async fn check_merge(
    state: &AppState,
    repo_id: i64,
    src_branch: &str,
    tgt_branch: &str,
) -> AppResult<(Option<String>, Vec<ChangeRow>, Vec<ChangeRow>, Vec<String>)> {
    let src_head = head_of(&state.db, repo_id, src_branch)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("源分支 {src_branch} 不存在")))?;
    let tgt_head = head_of(&state.db, repo_id, tgt_branch)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("目标分支 {tgt_branch} 不存在")))?;

    let base = merge_base(&state.db, repo_id, &src_head, &tgt_head).await?;
    let src_changes = net_changes(&state.db, repo_id, &src_head, base.as_deref()).await?;
    let tgt_changes = net_changes(&state.db, repo_id, &tgt_head, base.as_deref()).await?;

    let tmap: HashMap<&str, &ChangeRow> =
        tgt_changes.iter().map(|c| (c.element_uid.as_str(), c)).collect();

    let mut conflicts = Vec::new();
    for c in &src_changes {
        if let Some(t) = tmap.get(c.element_uid.as_str()) {
            if t.new_blob != c.new_blob {
                conflicts.push(c.element_uid.clone());
            }
        }
    }
    Ok((base, src_changes, tgt_changes, conflicts))
}

pub async fn merge(
    state: &AppState,
    repo_id: i64,
    user_id: i64,
    src_branch: &str,
    tgt_branch: &str,
    title: &str,
) -> AppResult<MergeReport> {
    let (base, src_changes, _tgt_changes, conflicts) =
        check_merge(state, repo_id, src_branch, tgt_branch).await?;
    if !conflicts.is_empty() {
        return Err(AppError::Conflict(format!(
            "存在 {} 处冲突，无法自动合入：{}",
            conflicts.len(),
            conflicts.join(", ")
        )));
    }

    let src_head = head_of(&state.db, repo_id, src_branch).await?.unwrap();
    let tgt_head = head_of(&state.db, repo_id, tgt_branch).await?.unwrap();

    let now = chrono::Utc::now().timestamp_millis();
    let merge_commit = sha1_hex(&format!(
        "merge|{}|{}|{}|{}|{now}",
        repo_id, tgt_head, src_head, title
    ));

    let mut tx = state.db.begin().await?;
    let mut seq = 0u32;
    for c in &src_changes {
        match c.op.as_str() {
            "A" | "M" => {
                sqlx::query(
                    "INSERT INTO element(repo_id,element_uid,path,cls,blob_id) \
                     SELECT ?,element_uid,path,cls,blob_id FROM element WHERE repo_id=? AND element_uid=? \
                     ON DUPLICATE KEY UPDATE path=VALUES(path), blob_id=VALUES(blob_id)",
                )
                .bind(repo_id)
                .bind(repo_id)
                .bind(&c.element_uid)
                .execute(&mut *tx)
                .await?;
                // 以源分支元素在 content_blob 中的最新内容为准
                if let Some(nb) = &c.new_blob {
                    sqlx::query("UPDATE element SET blob_id=? WHERE repo_id=? AND element_uid=?")
                        .bind(nb)
                        .bind(repo_id)
                        .bind(&c.element_uid)
                        .execute(&mut *tx)
                        .await?;
                }
            }
            "D" => {
                sqlx::query("DELETE FROM element WHERE repo_id=? AND element_uid=?")
                    .bind(repo_id)
                    .bind(&c.element_uid)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("DELETE FROM ref_edge WHERE repo_id=? AND (src_uid=? OR tgt_uid=?)")
                    .bind(repo_id)
                    .bind(&c.element_uid)
                    .bind(&c.element_uid)
                    .execute(&mut *tx)
                    .await?;
            }
            _ => {}
        }
        sqlx::query(
            "INSERT INTO change_set(repo_id,commit_id,seq,element_uid,path,op,old_blob,new_blob) \
             VALUES (?,?,?,?,?,?,?,?)",
        )
        .bind(repo_id)
        .bind(&merge_commit)
        .bind(seq)
        .bind(&c.element_uid)
        .bind(&c.path)
        .bind(&c.op)
        .bind(&c.old_blob)
        .bind(&c.new_blob)
        .execute(&mut *tx)
        .await?;
        seq += 1;
    }

    sqlx::query(
        "INSERT INTO commit_node(repo_id,commit_id,parent_id,parent2_id,author_id,msg) \
         VALUES (?,?,?,?,?,?)",
    )
    .bind(repo_id)
    .bind(&merge_commit)
    .bind(&tgt_head)
    .bind(&src_head)
    .bind(user_id)
    .bind(format!("merge {} into {}: {}", src_branch, tgt_branch, title))
    .execute(&mut *tx)
    .await?;

    let r = sqlx::query(
        "UPDATE ref SET commit_id=?, updated_at=NOW(3) \
         WHERE repo_id=? AND name=? AND commit_id=?",
    )
    .bind(&merge_commit)
    .bind(repo_id)
    .bind(tgt_branch)
    .bind(&tgt_head)
    .execute(&mut *tx)
    .await?;
    if r.rows_affected() != 1 {
        return Err(AppError::Conflict("合入失败：目标分支在合入期间被他人推进".into()));
    }

    sqlx::query("INSERT INTO audit_log(repo_id,user_id,action,detail) VALUES (?,?,?,?)")
        .bind(repo_id)
        .bind(user_id)
        .bind("merge")
        .bind(format!("{src_branch} -> {tgt_branch} ({})", &merge_commit[..8]))
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    // 刷新缓存 + 广播
    let _: Result<(), _> = redis::cmd("SET")
        .arg(state.key_ref(repo_id, tgt_branch))
        .arg(&merge_commit)
        .query_async(&mut state.redis.clone())
        .await;
    let evt = serde_json::json!({
        "type": "merge", "repo_id": repo_id,
        "source": src_branch, "target": tgt_branch,
        "merge_commit": merge_commit, "by": user_id,
    })
    .to_string();
    let _: Result<(), _> = redis::cmd("PUBLISH")
        .arg(state.channel_repo(repo_id))
        .arg(evt)
        .query_async(&mut state.redis.clone())
        .await;

    Ok(MergeReport {
        merge_commit,
        target_branch: tgt_branch.to_string(),
        source_branch: src_branch.to_string(),
        base,
        applied_changes: src_changes.len(),
        conflicts,
    })
}