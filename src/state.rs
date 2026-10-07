use std::sync::Arc;

use redis::aio::ConnectionManager;
use sqlx::MySqlPool;

use crate::config::Config;
use crate::metamodel::Metamodel;

/// 全进程共享状态：MySQL 连接池 + Redis 连接(克隆安全) + 配置 + 元模型注册表
#[derive(Clone)]
pub struct AppState {
    pub db: MySqlPool,
    pub redis: ConnectionManager,
    pub redis_client: redis::Client,
    pub cfg: Arc<Config>,
    /// Ecore 原模型：属性编辑器"能编辑哪些字段"的唯一依据
    pub mm: Arc<Metamodel>,
}

impl AppState {
    pub fn channel_repo(&self, repo_id: i64) -> String {
        format!("repo:{repo_id}:events")
    }

    pub fn key_ref(&self, repo_id: i64, branch: &str) -> String {
        format!("repo:{repo_id}:ref:{branch}")
    }

    pub fn key_push_lock(&self, repo_id: i64, branch: &str) -> String {
        format!("repo:{repo_id}:lock:push:{branch}")
    }
}