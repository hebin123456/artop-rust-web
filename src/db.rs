//! schema 引导：启动时幂等执行 migrations/0001_init.sql
//!
//! 不依赖 sqlx-cli，避免 POC 还要额外装工具：把迁移文件编进二进制，
//! 按 ";" 切分后逐条执行（文件本身不含存储过程，切分是安全的）。

use sqlx::MySqlPool;

pub async fn init_schema(pool: &MySqlPool) -> anyhow::Result<()> {
    let raw = include_str!("../migrations/0001_init.sql");
    for stmt in split_statements(raw) {
        sqlx::query(&stmt).execute(pool).await?;
    }
    Ok(())
}

/// 去掉 `--` 注释行，按分号切分，返回非空语句
fn split_statements(sql: &str) -> Vec<String> {
    let cleaned: String = sql
        .lines()
        .map(|l| if l.trim_start().starts_with("--") { "" } else { l })
        .collect::<Vec<_>>()
        .join("\n");

    cleaned
        .split(';')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}