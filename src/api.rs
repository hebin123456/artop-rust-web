//! HTTP API：账号 / 仓库 / 成员权限 / 分支 / 推送 / 历史 / 影响分析 / 评审合入 / 元素编辑器
//!
//! 鉴权：除 register/login/health 外，均要求 Authorization: Bearer <jwt>。
//! 授权：每个涉及仓库的操作都过 rbac::require，角色不足直接 403。

use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

use crate::auth::{self, AuthUser};
use crate::editor;
use crate::error::{AppError, AppResult};
use crate::rbac;
use crate::review;
use crate::state::AppState;
use crate::vcs;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/register", post(register))
        .route("/api/login", post(login))
        .route("/api/me", get(me))
        .route("/api/repos", post(create_repo).get(list_repos))
        .route("/api/repos/:id/members", post(add_member).get(list_members))
        .route("/api/repos/:id/branches", post(create_branch).get(list_branches))
        .route("/api/repos/:id/push", post(push))
        .route("/api/repos/:id/log", get(log))
        .route("/api/repos/:id/diff", get(diff))
        .route("/api/repos/:id/impact", get(impact))
        .route("/api/repos/:id/audit", get(audit_tail))
        // 元素编辑器读取侧（ARTOP Edit：内容树 / 元素详情 / 搜索 / 类分布）
        .route("/api/repos/:id/tree", get(model_tree))
        .route("/api/repos/:id/elements", get(list_elements))
        .route("/api/repos/:id/elements/:uid", get(get_element))
        .route("/api/repos/:id/classes", get(list_classes))
        .route("/api/repos/:id/reviews", post(create_review).get(list_reviews))
        .route("/api/reviews/:rid", get(get_review))
        .route("/api/reviews/:rid/merge-check", get(merge_check))
        .route("/api/reviews/:rid/decide", post(decide))
        .route("/api/reviews/:rid/merge", post(merge_review))
        .route("/ws", get(crate::realtime::ws_handler))
        // 客户 ARXML 体量不确定：放宽请求体上限（POC 取 256MB）
        .layer(DefaultBodyLimit::max(256 * 1024 * 1024))
        .with_state(state)
}

// ===================== 账号 =====================

async fn health() -> Json<Value> {
    Json(json!({ "ok": true, "service": "artop-rust-web" }))
}

#[derive(Deserialize)]
struct RegisterReq {
    username: String,
    email: String,
    password: String,
}

async fn register(State(st): State<AppState>, Json(req): Json<RegisterReq>) -> AppResult<Json<Value>> {
    let username = req.username.trim();
    if username.is_empty() {
        return Err(AppError::BadRequest("用户名不能为空".into()));
    }
    if req.password.len() < 6 {
        return Err(AppError::BadRequest("口令至少 6 位".into()));
    }

    let hash = auth::hash_password(&req.password)?;
    let r = sqlx::query("INSERT INTO app_user(username,email,pass_hash) VALUES (?,?,?)")
        .bind(username)
        .bind(req.email.trim())
        .bind(hash)
        .execute(&st.db)
        .await
        .map_err(|e| match e {
            sqlx::Error::Database(ref d) if d.is_unique_violation() => {
                AppError::Conflict(format!("用户名 {username} 已被占用"))
            }
            other => AppError::from(other),
        })?;

    Ok(Json(json!({ "id": r.last_insert_id(), "username": username })))
}

#[derive(Deserialize)]
struct LoginReq {
    username: String,
    password: String,
}

async fn login(State(st): State<AppState>, Json(req): Json<LoginReq>) -> AppResult<Json<Value>> {
    let row = sqlx::query("SELECT id, username, pass_hash FROM app_user WHERE username=?")
        .bind(req.username.trim())
        .fetch_optional(&st.db)
        .await?;

    let row = row.ok_or_else(|| AppError::Unauthorized("用户名或口令错误".into()))?;
    let hash: String = row.get("pass_hash");
    if !auth::verify_password(&req.password, &hash) {
        return Err(AppError::Unauthorized("用户名或口令错误".into()));
    }
    let id: i64 = row.get("id");
    let username: String = row.get("username");
    let token = auth::encode_token(&st.cfg, id, &username)?;
    Ok(Json(json!({ "token": token, "user": { "id": id, "username": username } })))
}

async fn me(State(st): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    let row = sqlx::query("SELECT id, username, email, created_at FROM app_user WHERE id=?")
        .bind(user.id)
        .fetch_optional(&st.db)
        .await?
        .ok_or_else(|| AppError::NotFound("用户不存在".into()))?;
    Ok(Json(json!({
        "id": row.get::<i64, _>("id"),
        "username": row.get::<String, _>("username"),
        "email": row.get::<String, _>("email"),
        "created_at": row.get::<chrono::NaiveDateTime, _>("created_at").to_string(),
    })))
}

// ===================== 仓库 =====================

async fn create_repo(State(st): State<AppState>, user: AuthUser, Json(req): Json<CreateRepoReq>) -> AppResult<Json<Value>> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("仓库名不能为空".into()));
    }

    let mut tx = st.db.begin().await?;
    let r = sqlx::query("INSERT INTO repo(name,owner_id) VALUES (?,?)")
        .bind(name)
        .bind(user.id)
        .execute(&mut *tx)
        .await
        .map_err(|e| match e {
            sqlx::Error::Database(ref d) if d.is_unique_violation() => {
                AppError::Conflict(format!("仓库 {name} 已存在"))
            }
            other => AppError::from(other),
        })?;
    let repo_id = r.last_insert_id() as i64;

    // 创建者即 owner
    sqlx::query("INSERT INTO repo_member(repo_id,user_id,role) VALUES (?,?,'owner')")
        .bind(repo_id)
        .bind(user.id)
        .execute(&mut *tx)
        .await?;

    // 每个仓库默认一条 main 分支指针？留空，首次 push 时自动建分支
    sqlx::query("INSERT INTO audit_log(repo_id,user_id,action,detail) VALUES (?,?,'repo.create',?)")
        .bind(repo_id)
        .bind(user.id)
        .bind(name)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(Json(json!({ "id": repo_id, "name": name, "role": "owner" })))
}

async fn list_repos(State(st): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    let rows = sqlx::query(
        "SELECT r.id, r.name, m.role, r.created_at FROM repo r \
         JOIN repo_member m ON m.repo_id = r.id \
         WHERE m.user_id = ? ORDER BY r.id",
    )
    .bind(user.id)
    .fetch_all(&st.db)
    .await?;

    let list: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<i64, _>("id"),
                "name": r.get::<String, _>("name"),
                "role": r.get::<String, _>("role"),
                "created_at": r.get::<chrono::NaiveDateTime, _>("created_at").to_string(),
            })
        })
        .collect();
    Ok(Json(json!({ "repos": list })))
}

#[derive(Deserialize)]
struct CreateRepoReq {
    name: String,
}

#[derive(Deserialize)]
struct AddMemberReq {
    username: String,
    role: String,
}

async fn add_member(
    State(st): State<AppState>,
    user: AuthUser,
    Path(repo_id): Path<i64>,
    Json(req): Json<AddMemberReq>,
) -> AppResult<Json<Value>> {
    rbac::require(&st.db, repo_id, user.id, rbac::P_ADMIN).await?; // maintainer+
    if !rbac::is_valid_role(&req.role) {
        return Err(AppError::BadRequest(format!("非法角色 {}", req.role)));
    }
    if req.role == "owner" {
        return Err(AppError::BadRequest("owner 只能由创建者持有，请用 maintainer".into()));
    }

    let target: Option<i64> = sqlx::query_scalar("SELECT id FROM app_user WHERE username=?")
        .bind(req.username.trim())
        .fetch_optional(&st.db)
        .await?;
    let target = target.ok_or_else(|| AppError::NotFound(format!("用户 {} 不存在", req.username)))?;

    sqlx::query(
        "INSERT INTO repo_member(repo_id,user_id,role) VALUES (?,?,?) \
         ON DUPLICATE KEY UPDATE role=VALUES(role)",
    )
    .bind(repo_id)
    .bind(target)
    .bind(&req.role)
    .execute(&st.db)
    .await?;

    review::broadcast(&st, repo_id, json!({
        "type": "member", "action": "set", "user_id": target, "role": req.role, "by": user.id,
    }))
    .await;

    Ok(Json(json!({ "repo_id": repo_id, "user_id": target, "role": req.role })))
}

async fn list_members(
    State(st): State<AppState>,
    user: AuthUser,
    Path(repo_id): Path<i64>,
) -> AppResult<Json<Value>> {
    rbac::require(&st.db, repo_id, user.id, rbac::P_READ).await?;
    let rows = sqlx::query(
        "SELECT u.id, u.username, m.role, m.added_at FROM repo_member m \
         JOIN app_user u ON u.id = m.user_id WHERE m.repo_id=? \
         ORDER BY FIELD(m.role,'owner','maintainer','reviewer','developer','reader')",
    )
    .bind(repo_id)
    .fetch_all(&st.db)
    .await?;

    let members: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<i64, _>("id"),
                "username": r.get::<String, _>("username"),
                "role": r.get::<String, _>("role"),
                "added_at": r.get::<chrono::NaiveDateTime, _>("added_at").to_string(),
            })
        })
        .collect();
    Ok(Json(json!({ "repo_id": repo_id, "members": members })))
}

// ===================== 分支 =====================

#[derive(Deserialize)]
struct CreateBranchReq {
    name: String,
    from: String,
}

async fn create_branch(
    State(st): State<AppState>,
    user: AuthUser,
    Path(repo_id): Path<i64>,
    Json(req): Json<CreateBranchReq>,
) -> AppResult<Json<Value>> {
    rbac::require(&st.db, repo_id, user.id, rbac::P_ADMIN).await?; // 分支管理需 maintainer+
    let head = vcs::create_branch(&st, repo_id, user.id, req.name.trim(), req.from.trim()).await?;
    review::broadcast(&st, repo_id, json!({
        "type": "branch", "action": "create", "name": req.name, "from": req.from, "by": user.id,
    }))
    .await;
    Ok(Json(json!({ "name": req.name, "from": req.from, "head": head })))
}

async fn list_branches(
    State(st): State<AppState>,
    user: AuthUser,
    Path(repo_id): Path<i64>,
) -> AppResult<Json<Value>> {
    rbac::require(&st.db, repo_id, user.id, rbac::P_READ).await?;
    let rows = sqlx::query(
        "SELECT name, kind, commit_id, updated_at FROM ref WHERE repo_id=? ORDER BY name",
    )
    .bind(repo_id)
    .fetch_all(&st.db)
    .await?;
    let branches: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "name": r.get::<String, _>("name"),
                "kind": r.get::<String, _>("kind"),
                "head": r.get::<String, _>("commit_id"),
                "updated_at": r.get::<chrono::NaiveDateTime, _>("updated_at").to_string(),
            })
        })
        .collect();
    Ok(Json(json!({ "repo_id": repo_id, "branches": branches })))
}

// ===================== 推送 / 历史 =====================

async fn push(
    State(st): State<AppState>,
    user: AuthUser,
    Path(repo_id): Path<i64>,
    Json(req): Json<vcs::PushReq>,
) -> AppResult<Json<Value>> {
    rbac::require(&st.db, repo_id, user.id, rbac::P_WRITE).await?; // developer+
    let resp = vcs::push(&st, repo_id, user.id, req).await?;
    Ok(Json(serde_json::to_value(resp).unwrap_or(Value::Null)))
}

#[derive(Deserialize)]
struct LogQuery {
    branch: String,
}

async fn log(
    State(st): State<AppState>,
    user: AuthUser,
    Path(repo_id): Path<i64>,
    Query(q): Query<LogQuery>,
) -> AppResult<Json<Value>> {
    rbac::require(&st.db, repo_id, user.id, rbac::P_READ).await?;
    let commits = review::log(&st, repo_id, &q.branch).await?;
    Ok(Json(json!({ "branch": q.branch, "commits": commits })))
}

#[derive(Deserialize)]
struct DiffQuery {
    from: String,
    to: String,
}

async fn diff(
    State(st): State<AppState>,
    user: AuthUser,
    Path(repo_id): Path<i64>,
    Query(q): Query<DiffQuery>,
) -> AppResult<Json<Value>> {
    rbac::require(&st.db, repo_id, user.id, rbac::P_READ).await?;
    let d = vcs::diff(&st, repo_id, &q.from, &q.to).await?;
    Ok(Json(d))
}

#[derive(Deserialize)]
struct ImpactQuery {
    uid: String,
    #[serde(default)]
    depth: Option<i64>,
}

async fn impact(
    State(st): State<AppState>,
    user: AuthUser,
    Path(repo_id): Path<i64>,
    Query(q): Query<ImpactQuery>,
) -> AppResult<Json<Value>> {
    rbac::require(&st.db, repo_id, user.id, rbac::P_READ).await?;
    let r = vcs::impact(&st, repo_id, &q.uid, q.depth.unwrap_or(6)).await?;
    Ok(Json(r))
}

async fn audit_tail(
    State(st): State<AppState>,
    user: AuthUser,
    Path(repo_id): Path<i64>,
) -> AppResult<Json<Value>> {
    rbac::require(&st.db, repo_id, user.id, rbac::P_READ).await?;
    let rows = sqlx::query(
        "SELECT a.action, a.detail, a.at, u.username FROM audit_log a \
         LEFT JOIN app_user u ON u.id=a.user_id \
         WHERE a.repo_id=? ORDER BY a.id DESC LIMIT 50",
    )
    .bind(repo_id)
    .fetch_all(&st.db)
    .await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "action": r.get::<String, _>("action"),
                "detail": r.get::<String, _>("detail"),
                "by": r.get::<Option<String>, _>("username"),
                "at": r.get::<chrono::NaiveDateTime, _>("at").to_string(),
            })
        })
        .collect();
    Ok(Json(json!({ "repo_id": repo_id, "audit": items })))
}

// ===================== 元素编辑器（ARTOP Edit） =====================

#[derive(Deserialize)]
struct TreeQuery {
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

async fn model_tree(
    State(st): State<AppState>,
    user: AuthUser,
    Path(repo_id): Path<i64>,
    Query(q): Query<TreeQuery>,
) -> AppResult<Json<Value>> {
    rbac::require(&st.db, repo_id, user.id, rbac::P_READ).await?;
    let parent = q.parent.unwrap_or_default();
    let out = editor::tree(&st.db, repo_id, &parent, q.limit.unwrap_or(2000)).await?;
    Ok(Json(out))
}

#[derive(Deserialize)]
struct ElementQuery {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    cls: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    offset: Option<i64>,
}

async fn list_elements(
    State(st): State<AppState>,
    user: AuthUser,
    Path(repo_id): Path<i64>,
    Query(q): Query<ElementQuery>,
) -> AppResult<Json<Value>> {
    rbac::require(&st.db, repo_id, user.id, rbac::P_READ).await?;
    let qq = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let cls = q.cls.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let items = editor::search(&st.db, repo_id, qq, cls, q.limit.unwrap_or(100), q.offset.unwrap_or(0)).await?;
    Ok(Json(json!({ "repo_id": repo_id, "elements": items })))
}

async fn get_element(
    State(st): State<AppState>,
    user: AuthUser,
    Path((repo_id, uid)): Path<(i64, String)>,
) -> AppResult<Json<Value>> {
    rbac::require(&st.db, repo_id, user.id, rbac::P_READ).await?;
    let el = editor::get(&st.db, repo_id, &uid)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("元素 {uid} 不存在")))?;
    Ok(Json(json!({ "repo_id": repo_id, "element": el })))
}

async fn list_classes(
    State(st): State<AppState>,
    user: AuthUser,
    Path(repo_id): Path<i64>,
) -> AppResult<Json<Value>> {
    rbac::require(&st.db, repo_id, user.id, rbac::P_READ).await?;
    let classes = editor::classes(&st.db, repo_id).await?;
    Ok(Json(json!({ "repo_id": repo_id, "classes": classes })))
}

// ===================== 评审 / 合入 =====================

#[derive(Deserialize)]
struct CreateReviewReq {
    src_branch: String,
    tgt_branch: String,
    title: String,
}

async fn create_review(
    State(st): State<AppState>,
    user: AuthUser,
    Path(repo_id): Path<i64>,
    Json(req): Json<CreateReviewReq>,
) -> AppResult<Json<Value>> {
    let id = review::create(
        &st,
        repo_id,
        user.id,
        req.src_branch.trim(),
        req.tgt_branch.trim(),
        req.title.trim(),
    )
    .await?;
    let rv = review::get(&st, id).await?;
    Ok(Json(json!({ "review": rv })))
}

#[derive(Deserialize)]
struct ReviewListQuery {
    #[serde(default)]
    status: Option<String>,
}

async fn list_reviews(
    State(st): State<AppState>,
    user: AuthUser,
    Path(repo_id): Path<i64>,
    Query(q): Query<ReviewListQuery>,
) -> AppResult<Json<Value>> {
    rbac::require(&st.db, repo_id, user.id, rbac::P_READ).await?;
    let rows = review::list(&st, repo_id, q.status.as_deref()).await?;
    Ok(Json(json!({ "repo_id": repo_id, "reviews": rows })))
}

async fn get_review(
    State(st): State<AppState>,
    user: AuthUser,
    Path(rid): Path<i64>,
) -> AppResult<Json<Value>> {
    let rv = review::get(&st, rid).await?;
    rbac::require(&st.db, rv.repo_id, user.id, rbac::P_READ).await?;
    Ok(Json(json!({ "review": rv })))
}

async fn merge_check(
    State(st): State<AppState>,
    user: AuthUser,
    Path(rid): Path<i64>,
) -> AppResult<Json<Value>> {
    let rv = review::get(&st, rid).await?;
    rbac::require(&st.db, rv.repo_id, user.id, rbac::P_READ).await?;
    Ok(Json(review::merge_preview(&st, rid).await?))
}

#[derive(Deserialize)]
struct DecideReq {
    approve: bool,
}

async fn decide(
    State(st): State<AppState>,
    user: AuthUser,
    Path(rid): Path<i64>,
    Json(req): Json<DecideReq>,
) -> AppResult<Json<Value>> {
    let rv = review::decide(&st, rid, user.id, req.approve).await?;
    Ok(Json(json!({ "review": rv })))
}

async fn merge_review(
    State(st): State<AppState>,
    user: AuthUser,
    Path(rid): Path<i64>,
) -> AppResult<Json<Value>> {
    let report = review::merge(&st, rid, user.id).await?;
    Ok(Json(json!({ "merge": report })))
}