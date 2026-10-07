//! AUTOSAR 元模型注册表（由 autosar448.ecore 抽取，见 scripts/build_metamodel.py）。
//!
//! 属性编辑器"能编辑哪些字段"完全由元模型决定，与 ARTOP 里 EMF Edit 的行为一致：
//!   EClass             -> 元素类型（对应 ARXML 标签，如 AR-PACKAGE）
//!   EAttribute         -> 标量属性（eType 决定控件：string/integer/boolean/enum/date）
//!   EReference         -> 引用属性（containment=true 可展开为子元素；*Ref 为跨元素引用）
//!   lowerBound/upperBound -> 单值还是多值
//!   derived/transient  -> 只读（派生计算结果，不可直接改）

use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Clone, Deserialize)]
pub struct Feature {
    /// ecore 里的名字，如 packageRef
    pub f: String,
    /// ARXML 标签，如 PACKAGE-REF
    #[serde(default)]
    pub x: Option<String>,
    /// attr | ref
    pub k: String,
    /// eType 的简单类名：属性是 EDataType/EEnum 名，引用是目标 EClass 名
    #[serde(default)]
    pub t: Option<String>,
    #[serde(default)]
    pub lb: i64,
    #[serde(default = "one")]
    pub ub: i64,
    /// 复数形式 ARXML 标签（多值引用用）
    #[serde(default)]
    pub xp: Option<String>,
    #[serde(default)]
    pub c: bool,
    #[serde(default)]
    pub opp: Option<String>,
    #[serde(default)]
    pub dv: Option<String>,
    #[serde(default)]
    pub der: bool,
    #[serde(default)]
    pub tr: bool,
    #[serde(default)]
    pub id: bool,
}

fn one() -> i64 {
    1
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClassMeta {
    pub n: String,
    #[serde(default)]
    pub x: Option<String>,
    #[serde(default)]
    pub ab: bool,
    #[serde(default)]
    pub sup: Vec<String>,
    #[serde(default)]
    pub feat: Vec<Feature>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnumMeta {
    pub n: String,
    #[serde(default)]
    pub x: Option<String>,
    /// [ARXML 字面量, ecore 字面量名]
    #[serde(default)]
    pub lits: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DataTypeMeta {
    pub n: String,
    #[serde(default)]
    pub x: Option<String>,
    #[serde(default)]
    pub ic: Option<String>,
}

#[derive(Deserialize)]
struct Raw {
    #[serde(default)]
    source: Vec<String>,
    #[serde(default)]
    classes: HashMap<String, ClassMeta>,
    #[serde(default)]
    enums: HashMap<String, EnumMeta>,
    #[serde(default)]
    datatypes: HashMap<String, DataTypeMeta>,
}

pub struct Metamodel {
    pub source: Vec<String>,
    classes: HashMap<String, ClassMeta>,
    /// ARXML 标签 -> EClass 名
    by_arxml: HashMap<String, String>,
    enums: HashMap<String, EnumMeta>,
    datatypes: HashMap<String, DataTypeMeta>,
    /// 全模型出现过的结构特征名（ecore 名 / 单数标签 / 复数标签），
    /// 用于校验引用特征是不是 ecore 里真的存在，而不是拼错的垃圾键。
    feature_names: HashSet<String>,
}

impl Metamodel {
    /// 空注册表：ecore 缺失时的降级形态，属性编辑器退回"原始键值"模式。
    pub fn empty() -> Self {
        Self {
            source: vec![],
            classes: HashMap::new(),
            by_arxml: HashMap::new(),
            enums: HashMap::new(),
            datatypes: HashMap::new(),
            feature_names: HashSet::new(),
        }
    }

    pub fn load(path: &str) -> anyhow::Result<Self> {
        let txt = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("读取元模型 {path} 失败：{e}"))?;
        let raw: Raw = serde_json::from_str(&txt)?;

        let mut by_arxml = HashMap::new();
        let mut feature_names = HashSet::new();
        for (n, c) in &raw.classes {
            if let Some(x) = &c.x {
                by_arxml.entry(x.clone()).or_insert_with(|| n.clone());
            }
            for f in &c.feat {
                feature_names.insert(f.f.clone());
                if let Some(x) = &f.x {
                    feature_names.insert(x.clone());
                }
                if let Some(xp) = &f.xp {
                    feature_names.insert(xp.clone());
                }
            }
        }
        Ok(Self {
            source: raw.source,
            classes: raw.classes,
            by_arxml,
            enums: raw.enums,
            datatypes: raw.datatypes,
            feature_names,
        })
    }

    /// 元模型是否真的加载了。没加载时校验要整体降级，不能把仓库锁死。
    pub fn is_loaded(&self) -> bool {
        !self.classes.is_empty()
    }

    pub fn stats(&self) -> Value {
        json!({
            "classes": self.classes.len(),
            "enums": self.enums.len(),
            "datatypes": self.datatypes.len(),
            "source": self.source,
        })
    }

    pub fn class_by_arxml(&self, arxml: &str) -> Option<&ClassMeta> {
        self.by_arxml.get(arxml).and_then(|n| self.classes.get(n))
    }

    /// 拉平继承链上的全部结构特征：子类优先，同名只留最具体的一个。
    /// 返回 (定义它的 EClass 名, 特征)。
    pub fn flatten(&self, root: &str) -> Vec<(String, Feature)> {
        let mut out: Vec<(String, Feature)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut visited: HashSet<String> = HashSet::new();
        self.collect(root, &mut seen, &mut visited, &mut out);
        out
    }

    fn collect(
        &self,
        cn: &str,
        seen: &mut HashSet<String>,
        visited: &mut HashSet<String>,
        out: &mut Vec<(String, Feature)>,
    ) {
        if !visited.insert(cn.to_string()) {
            return;
        }
        if let Some(c) = self.classes.get(cn) {
            for f in &c.feat {
                if seen.insert(f.f.clone()) {
                    out.push((cn.to_string(), f.clone()));
                }
            }
            for s in &c.sup {
                self.collect(s, seen, visited, out);
            }
        }
    }

    /// `child` 是不是 `target` 的实例（沿 eSuperTypes 上溯，含自身）。
    pub fn is_instance_of(&self, child: &str, target: &str) -> bool {
        if child == target {
            return true;
        }
        let mut stack = vec![child.to_string()];
        let mut seen: HashSet<String> = HashSet::new();
        while let Some(cn) = stack.pop() {
            if !seen.insert(cn.clone()) {
                continue;
            }
            let Some(c) = self.classes.get(&cn) else {
                continue;
            };
            for s in &c.sup {
                if s == target {
                    return true;
                }
                stack.push(s.clone());
            }
        }
        false
    }

    /// 父类 `parent` 能否以 containment 容纳 `child`：返回可用的 ARXML 标签；空表示不允许。
    /// 判据完全来自 ecore —— 特征的 containment=true 且目标类型是 child 的祖先。
    pub fn containment_tags(&self, parent: &str, child: &str) -> Vec<String> {
        self.flatten(parent)
            .into_iter()
            .filter(|(_, f)| f.k == "ref" && f.c)
            .filter(|(_, f)| {
                f.t.as_deref()
                    .is_some_and(|t| self.is_instance_of(child, t))
            })
            .map(|(_, f)| f.x.clone().or(f.xp.clone()).unwrap_or(f.f))
            .collect()
    }

    /// 父类声明的全部 containment 选项 (标签, 目标类型)，用于报错时给出可选项。
    pub fn containment_options(&self, parent: &str) -> Vec<(String, String)> {
        self.flatten(parent)
            .into_iter()
            .filter(|(_, f)| f.k == "ref" && f.c)
            .map(|(_, f)| {
                (
                    f.x.clone().or(f.xp.clone()).unwrap_or_else(|| f.f.clone()),
                    f.t.clone().unwrap_or_else(|| "EObject".into()),
                )
            })
            .collect()
    }

    /// `target` 的具体（非抽象）子类，用于"抽象类不能实例化"的提示。
    pub fn concrete_subclasses(&self, target: &str) -> Vec<String> {
        let mut v: Vec<String> = self
            .classes
            .values()
            .filter(|c| !c.ab && self.is_instance_of(&c.n, target))
            .map(|c| c.n.clone())
            .collect();
        v.sort();
        v
    }

    /// 该名字是否是 ecore 里出现过的结构特征（ecore 名 / 单数标签 / 复数标签）。
    pub fn has_feature_named(&self, name: &str) -> bool {
        self.feature_names.contains(name)
    }

    /// 类型信息 -> (控件类别, 枚举候选值)
    pub fn type_info(&self, t: Option<&str>) -> (String, Vec<String>) {
        let t = match t {
            Some(t) if !t.is_empty() => t,
            _ => return ("string".into(), vec![]),
        };
        if let Some(e) = self.enums.get(t) {
            let lits = e
                .lits
                .iter()
                .map(|l| l.first().cloned().unwrap_or_default())
                .collect();
            return ("enum".into(), lits);
        }
        if let Some(d) = self.datatypes.get(t) {
            return (java_kind(d.ic.as_deref()).into(), vec![]);
        }
        if self.classes.contains_key(t) {
            return ("element".into(), vec![]);
        }
        ("string".into(), vec![])
    }
}

fn java_kind(ic: Option<&str>) -> &'static str {
    match ic.unwrap_or("") {
        "java.lang.Integer" | "java.lang.Long" | "java.lang.Short" | "int" | "long" | "short" => {
            "integer"
        }
        "java.lang.Boolean" | "boolean" => "boolean",
        "java.lang.Double" | "java.lang.Float" | "double" | "float" => "number",
        "java.util.Date" => "date",
        _ => "string",
    }
}