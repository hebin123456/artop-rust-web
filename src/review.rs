//! 评审 / 合入（L6）：开 MR -> 他人审批 -> maintainer 合入目标分支
//!
//! 权限链：开单需 developer，审批需 reviewer，合入需 maintainer。
//! 关键约束：不能审批自己发起的 MR（职责分离）。

use serde::Serialize;
use sqlx::Row;

use crate::error::{AppError, AppResult};
use crate::rbac;
use crate::state::AppState;
use crate::vcs;

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ReviewRow {
    pub id: i64,
    pub repo_id: i64,
    pub src_branch: String,
    pub tgt_branch: String,
    pub head_commit: String,
    pub base_commit: Option<String>,
    pub title: String,
    pub status: String,
    pub author_id: i64,
    pub author: String,
    pub approver_id: Option<i64>,
    pub approver: Option<String>,
    pub created_at: chrono::NaiveDateTime,
    pub decided_at: Option<chrono::NaiveDateTime>,
}

const SELECT_REVIEW: &str = "SELECT rv.id, rv.repo_id, rv.src_branch, rv.tgt_branch, rv.head_commit, \
        rv.base_commit, rv.title, rv.status, rv.author_id, au.username AS author, \
        rv.approver_id, ap.username AS approver, rv.created_at, rv.decided_at \
     FROM review rv \
     JOIN app_user au ON au.id = rv.author_id \
     LEFT JOIN app_user ap ON ap.id = rv.approver_id";

pub async fn get(state: &AppState, review_id: i64) -> AppResult<ReviewRow> {
    sqlx::query_as::<_, ReviewRow>(&format!("{SELECT_REVIEW} WHERE rv.id=?"))
        .bind(review_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("评审 #{review_id} 不存在")))
}

/// 开评审：冻结源分支当前 HEAD，算出与目标的合并基点
pub async fn create(
    state: &AppState,
    repo_id: i64,
    user_id: i64,
    src_branch: &str,
    tgt_branch: &str,
    title: &str,
) -> AppResult<i64> {
    rbac::require(&state.db, repo_id, user_id, rbac::P_WRITE).await?;
    if src_branch == tgt_branch {
        return Err(AppError::BadRequest("源分支与目标分支不能相同".into()));
    }

    let src_head = vcs::head_of(&state.db, repo_id, src_branch)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("源分支 {src_branch} 不存在")))?;
    let tgt_head = vcs::head_of(&state.db, repo_id, tgt_branch)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("目标分支 {tgt_branch} 不存在")))?;

    let base = vcs::merge_base(&state.db, repo_id, &src_head, &tgt_head).await?;

    let r = sqlx::query(
        "INSERT INTO review(repo_id,src_branch,tgt_branch,head_commit,base_commit,title,status,author_id) \
         VALUES (?,?,?,?,?,?,'open',?)",
    )
    .bind(repo_id)
    .bind(src_branch)
    .bind(tgt_branch)
    .bind(&src_head)
    .bind(&base)
    .bind(title)
    .bind(user_id)
    .execute(&state.db)
    .await?;

    let id = r.last_insert_id() as i64;

    audit(state, repo_id, user_id, "review.open", &format!("#{id} {src_branch} -> {tgt_branch}")).await;
    broadcast(state, repo_id, serde_json::json!({
        "type": "review", "action": "open", "review_id": id,
        "source": src_branch, "target": tgt_branch, "by": user_id,
    }))
    .await;

    Ok(id)
}

pub async fn list(state: &AppState, repo_id: i64, status: Option<&str>) -> AppResult<Vec<ReviewRow>> {
    let rows = match status {
        Some(s) => {
            sqlx::query_as::<_, ReviewRow>(&format!("{SELECT_REVIEW} WHERE rv.repo_id=? AND rv.status=? ORDER BY rv.id DESC"))
                .bind(repo_id)
                .bind(s)
                .fetch_all(&state.db)
                .await?
        }
        None => {
            sqlx::query_as::<_, ReviewRow>(&format!("{SELECT_REVIEW} WHERE rv.repo_id=? ORDER BY rv.id DESC"))
                .bind(repo_id)
                .fetch_all(&state.db)
                .await?
        }
    };
    Ok(rows)
}

/// 审批：approve=true 通过 / false 驳回。需要 reviewer 及以上，且不得自审。
pub async fn decide(
    state: &AppState,
    review_id: i64,
    user_id: i64,
    approve: bool,
) -> AppResult<ReviewRow> {
    let rv = get(state, review_id).await?;
    rbac::require(&state.db, rv.repo_id, user_id, rbac::P_REVIEW).await?;

    if rv.status != "open" {
        return Err(AppError::Conflict(format!("评审当前状态为 {}，不可再审", rv.status)));
    }
    if rv.author_id == user_id {
        return Err(AppError::Forbidden("不能审批自己发起的评审（职责分离）".into()));
    }

    let status = if approve { "approved" } else { "rejected" };
    sqlx::query("UPDATE review SET status=?, approver_id=?, decided_at=NOW(3) WHERE id=? AND status='open'")
        .bind(status)
        .bind(user_id)
        .bind(review_id)
        .execute(&state.db)
        .await?;

    audit(state, rv.repo_id, user_id, "review.decide", &format!("#{review_id} -> {status}")).await;
    broadcast(state, rv.repo_id, serde_json::json!({
        "type": "review", "action": status, "review_id": review_id,
        "source": rv.src_branch, "target": rv.tgt_branch, "by": user_id,
    }))
    .await;

    get(state, review_id).await
}

/// 预演合入：只报冲突，不落库（评审页先看能不能合）
pub async fn merge_preview(state: &AppState, review_id: i64) -> AppResult<serde_json::Value> {
    let rv = get(state, review_id).await?;
    let (base, src_changes, tgt_changes, conflicts) =
        vcs::check_merge(state, rv.repo_id, &rv.src_branch, &rv.tgt_branch).await?;

    // 冻结头是否被推进：ireview 开单后源分支又提交过，需要重新评估
    let src_head = vcs::head_of(&state.db, rv.repo_id, &rv.src_branch).await?;
    let stale = src_head.as_deref() != Some(rv.head_commit.as_str());

    Ok(serde_json::json!({
        "review_id": review_id,
        "source": rv.src_branch,
        "target": rv.tgt_branch,
        "base": base,
        "source_changes": src_changes.len(),
        "target_changes": tgt_changes.len(),
        "conflicts": conflicts,
        "conflict_count": conflicts.len(),
        "mergeable": conflicts.is_empty(),
        "stale": stale,
        "frozen_head": rv.head_commit,
        "current_head": src_head,
    }))
}

/// 合入：评审必须是 approved，需要 maintainer 及以上
pub async fn merge(state: &AppState, review_id: i64, user_id: i64) -> AppResult<vcs::MergeReport> {
    let rv = get(state, review_id).await?;
    rbac::require(&state.db, rv.repo_id, user_id, rbac::P_MERGE).await?;

    if rv.status != "approved" {
        return Err(AppError::Conflict(format!(
            "评审状态为 {}，只有 approved 才能合入",
            rv.status
        )));
    }

    let report = vcs::merge(
        state,
        rv.repo_id,
        user_id,
        &rv.src_branch,
        &rv.tgt_branch,
        &rv.title,
    )
    .await?;

    sqlx::query("UPDATE review SET status='merged', decided_at=NOW(3) WHERE id=?")
        .bind(review_id)
        .execute(&state.db)
        .await?;

    audit(
        state,
        rv.repo_id,
        user_id,
        "review.merge",
        &format!("#{review_id} {} ({})", rv.src_branch, &report.merge_commit[..8]),
    )
    .await;

    Ok(report)
}

/// 提交日志：沿分支头回溯，带作者名
pub async fn log(state: &AppState, repo_id: i64, branch: &str) -> AppResult<Vec<serde_json::Value>> {
    let head = vcs::head_of(&state.db, repo_id, branch)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("分支 {branch} 不存在")))?;

    let chain = vcs::change_path(&state.db, repo_id, &head, None).await?;
    let mut out = Vec::with_capacity(chain.len());
    for cid in chain {
        let row = sqlx::query(
            "SELECT c.commit_id, c.parent_id, c.parent2_id, c.msg, c.created_at, u.username \
             FROM commit_node c JOIN app_user u ON u.id=c.author_id \
             WHERE c.repo_id=? AND c.commit_id=?",
        )
        .bind(repo_id)
        .bind(&cid)
        .fetch_optional(&state.db)
        .await?;
        if let Some(r) = row {
            let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM change_set WHERE repo_id=? AND commit_id=?")
                .bind(repo_id)
                .bind(&cid)
                .fetch_one(&state.db)
                .await?;
            out.push(serde_json::json!({
                "commit_id": r.get::<String, _>("commit_id"),
                "parent": r.get::<Option<String>, _>("parent_id"),
                "parent2": r.get::<Option<String>, _>("parent2_id"),
                "message": r.get::<String, _>("msg"),
                "author": r.get::<String, _>("username"),
                "created_at": r.get::<chrono::NaiveDateTime, _>("created_at").to_string(),
                "changes": n,
            }));
        }
    }
    Ok(out)
}

async fn audit(state: &AppState, repo_id: i64, user_id: i64, action: &str, detail: &str) {
    let _ = sqlx::query("INSERT INTO audit_log(repo_id,user_id,action,detail) VALUES (?,?,?,?)")
        .bind(repo_id)
        .bind(user_id)
        .bind(action)
        .bind(detail)
        .execute(&state.db)
        .await;
}

pub async fn broadcast(state: &AppState, repo_id: i64, evt: serde_json::Value) {
    let _: Result<(), _> = redis::cmd("PUBLISH")
        .arg(state.channel_repo(repo_id))
        .arg(evt.to_string())
        .query_async(&mut state.redis.clone())
        .await;
}