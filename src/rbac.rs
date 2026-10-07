use sqlx::MySqlPool;

use crate::error::{AppError, AppResult};

/// 仓库级角色，数值越大权限越高
pub const R_READER: i32 = 1;
pub const R_DEVELOPER: i32 = 2;
pub const R_REVIEWER: i32 = 3;
pub const R_MAINTAINER: i32 = 4;
pub const R_OWNER: i32 = 5;

/// 动作所需的最低角色等级
pub const P_READ: i32 = R_READER;
pub const P_WRITE: i32 = R_DEVELOPER; // 提交 / 推送 / 开评审
pub const P_REVIEW: i32 = R_REVIEWER; // 审批通过或驳回
pub const P_MERGE: i32 = R_MAINTAINER; // 合入
pub const P_ADMIN: i32 = R_MAINTAINER; // 成员与分支管理

pub fn rank(role: &str) -> i32 {
    match role {
        "reader" => R_READER,
        "developer" => R_DEVELOPER,
        "reviewer" => R_REVIEWER,
        "maintainer" => R_MAINTAINER,
        "owner" => R_OWNER,
        _ => 0,
    }
}

pub fn is_valid_role(role: &str) -> bool {
    rank(role) > 0
}

pub async fn role_of(db: &MySqlPool, repo_id: i64, user_id: i64) -> AppResult<Option<String>> {
    let r: Option<String> =
        sqlx::query_scalar("SELECT role FROM repo_member WHERE repo_id=? AND user_id=?")
            .bind(repo_id)
            .bind(user_id)
            .fetch_optional(db)
            .await?;
    Ok(r)
}

/// 校验权限，返回调用者的角色
pub async fn require(db: &MySqlPool, repo_id: i64, user_id: i64, need: i32) -> AppResult<String> {
    let role = role_of(db, repo_id, user_id)
        .await?
        .ok_or_else(|| AppError::Forbidden("你不是该仓库成员".into()))?;
    if rank(&role) < need {
        return Err(AppError::Forbidden(format!(
            "权限不足：该操作需要 {} 及以上，你当前是 {role}",
            name_of(need)
        )));
    }
    Ok(role)
}

pub fn name_of(need: i32) -> &'static str {
    match need {
        P_REVIEW => "reviewer",
        P_MERGE => "maintainer",
        P_WRITE => "developer",
        _ => "reader",
    }
}