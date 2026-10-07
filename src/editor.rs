//! 元素编辑器（ARTOP Edit 样式）的读取侧。
//!
//! 编辑器只从版本库"当前视图层"(`element` + `content_blob` + `ref_edge`)读，
//! 写回仍走 `vcs::push`（内容寻址 blob + 提交 DAG + 分支 CAS），
//! 因此"在编辑器里改一个元素"和"用命令行 push 一个变更"是同一套语义。
//!
//! 提供的三块能力，正好对应 ARTOP Edit 的界面结构：
//!   - `tree()`    -> AutosarContentsTreePage：按路径分段懒加载的内容树
//!   - `get()`     -> OverviewPage：元素元数据 + 属性 + 出/入向引用
//!   - `search()`  -> 属性页里的"查找元素"下拉框

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{json, Value};
use sqlx::{MySqlPool, Row};

use crate::error::AppResult;

#[derive(Debug, Serialize)]
pub struct ElementBrief {
    pub uid: String,
    pub path: String,
    pub cls: String,
    pub sn: String,
}

/// 转义 LIKE 通配符：ARXML 路径里 `_` 很常见（如 `ApplicationDataTypes_Blueprint`），
/// 不转义会被当成"任意单字符"，从而匹配到错误的一批元素。
fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

fn brief(row: &sqlx::mysql::MySqlRow) -> ElementBrief {
    ElementBrief {
        uid: row.get("element_uid"),
        path: row.get("path"),
        cls: row.get("cls"),
        sn: row.get("sn"),
    }
}

// ===================== 搜索（属性页的查找） =====================

pub async fn search(
    db: &MySqlPool,
    repo_id: i64,
    q: Option<&str>,
    cls: Option<&str>,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<ElementBrief>> {
    let limit = limit.clamp(1, 500);
    let offset = offset.max(0);

    let mut sql = String::from(
        "SELECT e.element_uid, e.path, e.cls, b.sn \
         FROM element e JOIN content_blob b \
           ON b.repo_id = e.repo_id AND b.blob_id = e.blob_id \
         WHERE e.repo_id = ?",
    );
    if q.is_some() {
        sql += " AND (e.element_uid LIKE ? ESCAPE '\\\\' \
                 OR e.path LIKE ? ESCAPE '\\\\' \
                 OR b.sn LIKE ? ESCAPE '\\\\')";
    }
    if cls.is_some() {
        sql += " AND e.cls = ?";
    }
    sql += " ORDER BY e.path LIMIT ? OFFSET ?";

    let mut query = sqlx::query(&sql).bind(repo_id);
    if let Some(q) = q {
        let like = format!("%{}%", like_escape(q));
        query = query.bind(like.clone()).bind(like.clone()).bind(like);
    }
    if let Some(c) = cls {
        query = query.bind(c);
    }
    let rows = query.bind(limit).bind(offset).fetch_all(db).await?;

    Ok(rows.iter().map(brief).collect())
}

/// 类分布：给属性页的分类下拉框用。
pub async fn classes(db: &MySqlPool, repo_id: i64) -> AppResult<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT cls, COUNT(*) AS c FROM element WHERE repo_id=? GROUP BY cls ORDER BY c DESC, cls",
    )
    .bind(repo_id)
    .fetch_all(db)
    .await?;
    Ok(rows
        .iter()
        .map(|r| json!({ "cls": r.get::<String, _>("cls"), "count": r.get::<i64, _>("c") }))
        .collect())
}

// ===================== 内容树（AutosarContentsTreePage） =====================

/// 懒加载：只返回 `parent` 的**直接**子节点。
///
/// 子节点有两种：
///   - `is_element = true`：该路径下真的存在一个元素（可点开属性页）
///   - `has_children = true`：还有更深层的路径，需要继续展开
/// 两者可同时为真（包本身既是元素、又含子元素）。
pub async fn tree(db: &MySqlPool, repo_id: i64, parent: &str, limit: i64) -> AppResult<Value> {
    let limit = limit.clamp(1, 5000);
    let parent = parent.trim_end_matches('/');

    // base 为父路径（根为空串）；like 为"父路径下的所有后代"前缀
    let (base, like) = if parent.is_empty() {
        (String::new(), "/%".to_string())
    } else {
        (parent.to_string(), format!("{}/%", like_escape(parent)))
    };

    let rows = sqlx::query(
        "SELECT e.element_uid, e.path, e.cls, b.sn \
         FROM element e JOIN content_blob b \
           ON b.repo_id = e.repo_id AND b.blob_id = e.blob_id \
         WHERE e.repo_id = ? AND e.path LIKE ? ESCAPE '\\\\' \
         ORDER BY e.path LIMIT ?",
    )
    .bind(repo_id)
    .bind(&like)
    .bind(limit)
    .fetch_all(db)
    .await?;

    let prefix = if base.is_empty() {
        String::new()
    } else {
        format!("{base}/")
    };

    let mut map: BTreeMap<String, Value> = BTreeMap::new();
    for r in &rows {
        let path: String = r.get("path");
        // 根节点时 prefix 为空，路径形如 "/AUTOSAR"，需去掉前导 '/'
        let rest = match path.strip_prefix(&prefix) {
            Some(s) => s.trim_start_matches('/'),
            None => continue,
        };
        let seg = match rest.split('/').next() {
            Some(s) if !s.is_empty() => s,
            _ => continue,
        };
        let child_path = format!("{base}/{seg}");

        let e = map.entry(child_path.clone()).or_insert_with(|| {
            json!({
                "name": seg, "path": child_path,
                "is_element": false, "has_children": false,
                "uid": Value::Null, "cls": Value::Null, "sn": Value::Null,
            })
        });

        if rest == seg {
            // 这条路径本身就是一个元素
            e["is_element"] = json!(true);
            e["uid"] = json!(r.get::<String, _>("element_uid"));
            e["cls"] = json!(r.get::<String, _>("cls"));
            let sn: String = r.get("sn");
            e["sn"] = json!(if sn.is_empty() { seg } else { &sn });
        } else {
            e["has_children"] = json!(true);
        }
    }

    let nodes: Vec<Value> = map.into_values().collect();
    Ok(json!({
        "parent": parent,
        "nodes": nodes,
        "truncated": rows.len() as i64 >= limit,
    }))
}

// ===================== 元素详情（OverviewPage） =====================

/// 引用查询：out=true 查"我引用了谁"，out=false 查"谁引用了我"。
/// 顺带回填对端元素的 path/sn/cls，让界面能直接显示可读名字而不是裸 uid。
async fn refs_dir(db: &MySqlPool, repo_id: i64, uid: &str, out: bool) -> AppResult<Vec<Value>> {
    let (join_col, where_col) = if out {
        ("r.tgt_uid", "r.src_uid")
    } else {
        ("r.src_uid", "r.tgt_uid")
    };
    let sql = format!(
        "SELECT r.feat AS feat, {join_col} AS other, e.path AS path, b.sn AS sn, e.cls AS cls \
         FROM ref_edge r \
         LEFT JOIN element e ON e.repo_id = r.repo_id AND e.element_uid = {join_col} \
         LEFT JOIN content_blob b ON b.repo_id = e.repo_id AND b.blob_id = e.blob_id \
         WHERE r.repo_id = ? AND {where_col} = ? \
         ORDER BY r.feat"
    );
    let rows = sqlx::query(&sql)
        .bind(repo_id)
        .bind(uid)
        .fetch_all(db)
        .await?;

    Ok(rows
        .iter()
        .map(|r| {
            json!({
                "feat": r.get::<String, _>("feat"),
                "tgt": r.get::<String, _>("other"),
                "path": r.get::<Option<String>, _>("path"),
                "sn": r.get::<Option<String>, _>("sn"),
                "cls": r.get::<Option<String>, _>("cls"),
            })
        })
        .collect())
}

pub async fn get(db: &MySqlPool, repo_id: i64, uid: &str) -> AppResult<Option<Value>> {
    let row = sqlx::query(
        "SELECT e.element_uid, e.path, e.cls, b.sn, b.body, b.blob_id \
         FROM element e JOIN content_blob b \
           ON b.repo_id = e.repo_id AND b.blob_id = e.blob_id \
         WHERE e.repo_id = ? AND e.element_uid = ?",
    )
    .bind(repo_id)
    .bind(uid)
    .fetch_optional(db)
    .await?;

    let row = match row {
        Some(r) => r,
        None => return Ok(None),
    };

    let body: Value = row.get("body");
    let attrs = body.get("attrs").cloned().unwrap_or_else(|| json!({}));

    Ok(Some(json!({
        "uid": row.get::<String, _>("element_uid"),
        "path": row.get::<String, _>("path"),
        "cls": row.get::<String, _>("cls"),
        "sn": row.get::<String, _>("sn"),
        "blob_id": row.get::<String, _>("blob_id"),
        "attrs": attrs,
        "refs_out": refs_dir(db, repo_id, uid, true).await?,
        "refs_in": refs_dir(db, repo_id, uid, false).await?,
    })))
}