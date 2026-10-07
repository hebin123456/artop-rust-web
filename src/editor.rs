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

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use serde_json::{json, Value};
use sqlx::{MySqlPool, Row};

use crate::error::{AppError, AppResult};
use crate::metamodel::{Feature, Metamodel};

#[derive(Debug, Serialize)]
pub struct ElementBrief {
    pub uid: String,
    pub path: String,
    pub cls: String,
    pub sn: String,
}

/// 转义 LIKE 通配符：ARXML 路径里 `_` 很常见（如 `ApplicationDataTypes_Blueprint`），
/// 不转义会被当成"任意单字符"，从而匹配到错误的一批元素。
pub(crate) fn like_escape(s: &str) -> String {
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
///
/// 分组在 SQL 里完成：对"父路径之后的第一段"做 GROUP BY，只回传每段一行。
/// 早先的"先取 limit 行后代、再在内存里分组"会被 ORDER BY path 的窗口吃满——
/// 大仓库里所有名额都被字母序最靠前的那个分支占掉，其余分支被静默丢弃，
/// 模型树于是看起来"没有内容"（如 8281 个元素的 EcucDefs 分支整个消失）。
pub async fn tree(db: &MySqlPool, repo_id: i64, parent: &str, limit: i64) -> AppResult<Value> {
    let limit = limit.clamp(1, 5000);
    let parent = parent.trim_end_matches('/');

    // prefix 为父路径（根为空串）；like 为"父路径下的所有后代"前缀
    let prefix = parent.to_string();
    let like = if parent.is_empty() {
        "/%".to_string()
    } else {
        format!("{}/%", like_escape(parent))
    };

    // SUBSTRING(path, CHAR_LENGTH(prefix) + 2) 正好跳过 "父路径/" 落到下一段
    let rows = sqlx::query(
        "SELECT t.seg AS seg, t.has_children AS has_children, \
                e.element_uid AS uid, e.cls AS cls, b.sn AS sn \
         FROM ( \
           SELECT SUBSTRING_INDEX(SUBSTRING(path, CHAR_LENGTH(?) + 2), '/', 1) AS seg, \
                  CAST(MAX(CASE WHEN path <> CONCAT(?, '/', \
                       SUBSTRING_INDEX(SUBSTRING(path, CHAR_LENGTH(?) + 2), '/', 1)) \
                       THEN 1 ELSE 0 END) AS SIGNED) AS has_children \
           FROM element \
           WHERE repo_id = ? AND path LIKE ? ESCAPE '\\\\' \
           GROUP BY seg ORDER BY seg LIMIT ? \
         ) t \
         LEFT JOIN element e ON e.repo_id = ? AND e.path = CONCAT(?, '/', t.seg) \
         LEFT JOIN content_blob b ON b.repo_id = e.repo_id AND b.blob_id = e.blob_id \
         ORDER BY t.seg",
    )
    .bind(&prefix)
    .bind(&prefix)
    .bind(&prefix)
    .bind(repo_id)
    .bind(&like)
    .bind(limit)
    .bind(repo_id)
    .bind(&prefix)
    .fetch_all(db)
    .await?;

    let mut nodes: Vec<Value> = Vec::with_capacity(rows.len());
    for r in &rows {
        let seg: String = r.get("seg");
        let has_children: i64 = r.get("has_children");
        let uid: Option<String> = r.get("uid");
        let cls: Option<String> = r.get("cls");
        let sn: Option<String> = r.get("sn");
        let child_path = format!("{prefix}/{seg}");
        nodes.push(json!({
            "name": seg,
            "path": child_path,
            // LEFT JOIN 命中说明该路径下确有元素；否则只是"路径经过的中间层"
            "is_element": uid.is_some(),
            "has_children": has_children != 0,
            "uid": uid,
            "cls": cls,
            "sn": sn.filter(|s| !s.is_empty()).unwrap_or_else(|| seg.clone()),
        }));
    }

    // ARTOP 的内容树天然以 AUTOSAR 文档根为根节点：AutosarFormEditor.getModelRoot()
    // 拿到的就是那个 GAUTOSAR，树只是把它当 pageInput 画出来，跟仓库里有没有内容无关。
    // 所以根层级一定得有 AUTOSAR：已经建过就用地道的那个元素，还没建过（空仓库）就摆一个
    // "待创建"的合成根，用户从它开始长出 AR-PACKAGE，而不是面对一棵空树无从下手。
    if prefix.is_empty() {
        match nodes.iter_mut().find(|n| n["name"] == "AUTOSAR") {
            Some(n) => n["root"] = json!(true),
            None => nodes.insert(
                0,
                json!({
                    "name": "AUTOSAR",
                    "path": "/AUTOSAR",
                    "is_element": false,
                    "has_children": false,
                    "uid": Value::Null,
                    "cls": "AUTOSAR",
                    "sn": "AUTOSAR",
                    "root": true,
                }),
            ),
        }
        // 稳定排序把 AUTOSAR 顶到第一位（其余顶层段只可能是历史遗留，保留可见不藏）
        nodes.sort_by_key(|n| !n["root"].as_bool().unwrap_or(false));
    }

    let truncated = nodes.len() as i64 >= limit;
    Ok(json!({
        "parent": parent,
        "nodes": nodes,
        "truncated": truncated,
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

// ===================== 属性树 schema（元模型驱动） =====================

fn is_empty_attr(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

/// 取属性的当前值：先认特殊字段（SHORT-NAME/UUID），再按 ARXML 标签、ecore 名去 attrs 里找。
///
/// 返回 (值, 命中的原始键)。键要带回去，因为 blob 里的 attrs 键可能是 ARXML 标签、
/// 也可能是 ecore 名，编辑器写回时必须沿用同一个键，否则会出现"改完没生效/多出一份"。
fn attr_value(
    alow: &HashMap<String, Value>,
    raw_keys: &HashMap<String, String>,
    uid: &str,
    sn: &str,
    f: &Feature,
) -> (Value, Option<String>) {
    match f.x.as_deref() {
        Some("SHORT-NAME") => return (json!(sn), Some("SHORT-NAME".to_string())),
        Some("UUID") => return (json!(uid), Some("UUID".to_string())),
        _ => {}
    }
    for cand in [f.x.as_deref(), Some(f.f.as_str())] {
        if let Some(c) = cand {
            let lc = c.to_lowercase();
            if let Some(v) = alow.get(&lc) {
                return (v.clone(), raw_keys.get(&lc).cloned());
            }
        }
    }
    (Value::Null, None)
}

/// 属性编辑器 schema：把元素映射到元模型，回答"这个类能编辑哪些字段、当前值是什么"。
///
/// 返回结构直接对应前端的属性树：
///   metamodel      -> 这个元素属于哪个 EClass、继承链、来源 ecore
///   groups[attr]   -> 可编辑标量属性（类型/多重性/枚举候选/是否只读）
///   groups[ref]    -> 引用属性（含既有目标元素，可点进去）
pub async fn schema(
    db: &MySqlPool,
    mm: &Metamodel,
    repo_id: i64,
    uid: &str,
) -> AppResult<Value> {
    let row = sqlx::query(
        "SELECT e.element_uid, e.path, e.cls, b.sn, b.body \
         FROM element e JOIN content_blob b \
           ON b.repo_id = e.repo_id AND b.blob_id = e.blob_id \
         WHERE e.repo_id = ? AND e.element_uid = ?",
    )
    .bind(repo_id)
    .bind(uid)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| AppError::NotFound(format!("元素 {uid} 不存在")))?;

    let cls: String = row.get("cls");
    let sn: String = row.get("sn");
    let path: String = row.get("path");
    let body: Value = row.get("body");
    let attrs = body.get("attrs").cloned().unwrap_or_else(|| json!({}));

    let mut alow: HashMap<String, Value> = HashMap::new();
    // 小写键 -> 原始键，写回时沿用原键，避免大小写/别名造成重复字段
    let mut raw_keys: HashMap<String, String> = HashMap::new();
    if let Some(o) = attrs.as_object() {
        for (k, v) in o {
            let lc = k.to_lowercase();
            alow.insert(lc.clone(), v.clone());
            raw_keys.entry(lc).or_insert_with(|| k.clone());
        }
    }

    // 出向引用按 ARXML 标签归组
    let refs = refs_dir(db, repo_id, uid, true).await?;
    let mut ref_by_feat: HashMap<String, Vec<Value>> = HashMap::new();
    for r in &refs {
        if let Some(feat) = r.get("feat").and_then(|v| v.as_str()) {
            ref_by_feat.entry(feat.to_string()).or_default().push(r.clone());
        }
    }

    let e = expand(mm, &cls, &alow, &raw_keys, uid, &sn, &ref_by_feat);

    // 元模型之外的自定义键（历史遗留 / 客户私有字段）：仍然可见可改，不丢数据
    let mut extra: Vec<Value> = Vec::new();
    if let Some(o) = attrs.as_object() {
        for (k, v) in o {
            if e.matched.contains(&k.to_lowercase()) {
                continue;
            }
            extra.push(json!({
                "name": k, "label": k, "key": k, "kind": "extra",
                "category": "string", "value": v, "hasValue": true,
                "multi": false, "readOnly": false,
            }));
        }
    }

    Ok(json!({
        "repo_id": repo_id,
        "element": { "uid": uid, "path": path, "cls": cls, "sn": sn, "attrs": attrs },
        "metamodel": {
            "class": e.cname,
            "arxml": e.carxml,
            "abstract": e.cabstract,
            "supers": e.csupers,
            "source": mm.source,
            "resolved": e.resolved,
            "featureCount": e.attrs.len() + e.refs.len(),
        },
        "groups": [
            { "id": "attr", "title": "属性 Attributes", "items": e.attrs },
            { "id": "ref",  "title": "引用 References", "items": e.refs },
            { "id": "extra", "title": "其他键 Others", "items": extra },
        ],
    }))
}

/// 元模型静态形态：只回答"这个类声明了哪些可编辑字段"，不带任何当前值。
/// 供"新建元素"按类名预取属性树用，这样还没入库的新元素也能看到 ecore 声明的字段。
pub fn class_schema(mm: &Metamodel, cls: &str) -> Value {
    let e = expand(mm, cls, &HashMap::new(), &HashMap::new(), "", "", &HashMap::new());
    json!({
        "metamodel": {
            "class": e.cname,
            "arxml": e.carxml,
            "abstract": e.cabstract,
            "supers": e.csupers,
            "source": mm.source,
            "resolved": e.resolved,
            "featureCount": e.attrs.len() + e.refs.len(),
        },
        "groups": [
            { "id": "attr", "title": "属性 Attributes", "items": e.attrs },
            { "id": "ref",  "title": "引用 References", "items": e.refs },
            { "id": "extra", "title": "其他键 Others", "items": [] },
        ],
    })
}

/// 一次元模型展开的结果
struct Expanded {
    cname: String,
    carxml: String,
    cabstract: bool,
    csupers: Vec<String>,
    resolved: bool,
    attrs: Vec<Value>,
    refs: Vec<Value>,
    /// 被元模型特征命中过的原始 attrs 键（小写）
    matched: HashSet<String>,
}

/// 拉开一个 EClass 的继承链，生成属性树的 item 列表。
/// 传空的 alow/ref_by_feat 就是"静态形态"（新建元素），传真实数据就是"带当前值"。
#[allow(clippy::too_many_arguments)]
fn expand(
    mm: &Metamodel,
    cls: &str,
    alow: &HashMap<String, Value>,
    raw_keys: &HashMap<String, String>,
    uid: &str,
    sn: &str,
    ref_by_feat: &HashMap<String, Vec<Value>>,
) -> Expanded {
    let class = mm.class_by_arxml(cls);
    let (cname, carxml, cabstract, csupers) = match class {
        Some(c) => (
            c.n.clone(),
            c.x.clone().unwrap_or_else(|| cls.to_string()),
            c.ab,
            c.sup.clone(),
        ),
        None => (cls.to_string(), cls.to_string(), false, vec![]),
    };

    let (mut attr_filled, mut attr_empty) = (Vec::new(), Vec::new());
    let (mut ref_filled, mut ref_empty) = (Vec::new(), Vec::new());
    let mut matched: HashSet<String> = HashSet::new();
    // 被元模型引用特征命中过的 ARXML 标签；剩下的 refs_out 就是"元模型外"的引用
    let mut matched_refs: HashSet<String> = HashSet::new();

    if class.is_some() {
        for (definer, f) in mm.flatten(&cname) {
            let label = f.x.clone().unwrap_or_else(|| f.f.clone());
            let multi = f.ub < 0 || f.ub > 1;
            if f.k == "attr" {
                let (cat, lits) = mm.type_info(f.t.as_deref());
                let (v, vkey) = attr_value(alow, raw_keys, uid, sn, &f);
                if let Some(k) = &vkey {
                    matched.insert(k.to_lowercase());
                }
                let item = json!({
                    "name": f.f,
                    "label": label,
                    "kind": "attr",
                    "type": f.t,
                    "category": cat,
                    "enum": lits,
                    "lower": f.lb,
                    "upper": f.ub,
                    "multi": multi,
                    "default": f.dv,
                    "readOnly": f.der || f.tr,
                    "identifier": f.id,
                    "definedIn": definer,
                    // 写回时沿用的原始键；为 null 表示用 label 作为新键
                    "valueKey": vkey,
                    "hasValue": !is_empty_attr(&v),
                    "value": v,
                });
                if is_empty_attr(&v) {
                    attr_empty.push(item);
                } else {
                    attr_filled.push(item);
                }
            } else {
                matched_refs.insert(label.clone());
                let targets = ref_by_feat.get(&label).cloned().unwrap_or_default();
                let item = json!({
                    "name": f.f,
                    "label": label,
                    "labelPlural": f.xp,
                    "kind": "ref",
                    "type": f.t,
                    "category": "element",
                    "lower": f.lb,
                    "upper": f.ub,
                    "multi": multi,
                    "containment": f.c,
                    "opposite": f.opp,
                    "readOnly": f.der || f.tr,
                    "definedIn": definer,
                    "unmapped": false,
                    "targets": targets,
                    "count": targets.len(),
                });
                if targets.is_empty() {
                    ref_empty.push(item);
                } else {
                    ref_filled.push(item);
                }
            }
        }
    }
    attr_filled.append(&mut attr_empty);
    ref_filled.append(&mut ref_empty);

    // 元模型外的引用：feat 不在该类的 EReference 里（历史数据/私有标签），仍要显示，
    // 否则引用会在属性树里凭空消失。归到引用组末尾，并标 unmapped 供前端提示。
    let mut unmapped: Vec<Value> = ref_by_feat
        .iter()
        .filter(|(feat, _)| !matched_refs.contains(*feat))
        .map(|(feat, targets)| {
            json!({
                "name": feat, "label": feat, "kind": "ref", "type": Value::Null,
                "category": "element", "lower": 0, "upper": -1, "multi": true,
                "containment": false, "opposite": Value::Null, "readOnly": false,
                "definedIn": Value::Null, "unmapped": true,
                "targets": targets, "count": targets.len(),
            })
        })
        .collect();
    unmapped.sort_by(|a, b| {
        a["label"].as_str().unwrap_or("").cmp(b["label"].as_str().unwrap_or(""))
    });
    let refs = ref_filled
        .into_iter()
        .chain(unmapped)
        .collect::<Vec<_>>();

    Expanded {
        cname,
        carxml,
        cabstract,
        csupers,
        resolved: class.is_some(),
        attrs: attr_filled,
        refs,
        matched,
    }
}