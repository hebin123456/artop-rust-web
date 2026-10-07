#!/usr/bin/env python3
# 从 ARTOP 的 Ecore 原模型抽取「元模型注册表」，供属性编辑器使用。
#
# 为什么要抽：ARTOP 的属性编辑器（EMF Edit）里"能编辑哪些字段"完全由 Ecore 决定——
# 每个 EClass 的 EAttribute / EReference 就是可编辑属性，类型决定控件，
# lowerBound/upperBound 决定单值还是多值，containment 决定能不能展开成子节点。
# 我们把这份元模型转成紧凑 JSON，服务端启动时加载，用于给前端下发属性树 schema。
#
# 注意：ecore 里只有根元素带 ecore: 前缀，子元素（eClassifiers 等）是不带命名空间的，
# 所以要按 local-name 比较，不能用 {ns}tag。
#
# 用法: build_metamodel.py <autosar448.ecore> <out.json> [gautosar.ecore ...]
import sys
import json
import xml.etree.ElementTree as ET

XSI = "{http://www.w3.org/2001/XMLSchema-instance}"
EXTENDED_META = "http:///org/eclipse/emf/ecore/util/ExtendedMetaData"


def ln(tag):
    """去掉命名空间，取 local-name。"""
    return tag.rsplit("}", 1)[-1]


def kids(el, name):
    return [c for c in el if ln(c.tag) == name]


def xsi_kind(el):
    return (el.get(XSI + "type", "") or "").split(":")[-1]


def xml_name(el):
    """取 ARXML 标签名：优先 ExtendedMetaData 的 name，退回 TaggedValues 的 xml.name。"""
    fallback = None
    for ann in kids(el, "eAnnotations"):
        src = ann.get("source", "")
        for d in kids(ann, "details"):
            key, val = d.get("key"), d.get("value")
            if not val:
                continue
            if src == EXTENDED_META and key == "name":
                return val
            if key == "xml.name" and fallback is None:
                fallback = val
    return fallback


def detail(el, key):
    for ann in kids(el, "eAnnotations"):
        for d in kids(ann, "details"):
            if d.get("key") == key and d.get("value") is not None:
                return d.get("value")
    return None


def ref_simple(eType):
    """把 '#//pkg/sub/ClassName' 或 'gautosar.ecore#//x/Y' 解析成简单类名。"""
    if not eType:
        return None
    return eType.split("#")[-1].rstrip("/").split("/")[-1]


def parse_feature(f):
    kind = "attr" if xsi_kind(f) == "EAttribute" else "ref"
    d = {
        "f": f.get("name"),
        "x": xml_name(f),
        "k": kind,
        "t": ref_simple(f.get("eType")),
        "lb": int(f.get("lowerBound", "0")),
        "ub": int(f.get("upperBound", "1")),
    }
    xp = detail(f, "xml.namePlural")
    if xp:
        d["xp"] = xp
    if f.get("containment") == "true":
        d["c"] = True
    if f.get("eOpposite"):
        d["opp"] = ref_simple(f.get("eOpposite"))
    if f.get("defaultValueLiteral") is not None:
        d["dv"] = f.get("defaultValueLiteral")
    # 派生 / 瞬态属性不可直接编辑，前端要标灰
    if f.get("derived") == "true":
        d["der"] = True
    if f.get("transient") == "true":
        d["tr"] = True
    if f.get("iD") == "true":
        d["id"] = True
    return d


def walk(node, classes, enums, datatypes):
    for ch in node:
        name = ln(ch.tag)
        if name == "eClassifiers":
            kind = xsi_kind(ch)
            cn = ch.get("name")
            if kind == "EClass":
                classes[cn] = {
                    "n": cn,
                    "x": xml_name(ch),
                    "ab": ch.get("abstract") == "true",
                    "sup": [ref_simple(s) for s in (ch.get("eSuperTypes") or "").split() if s],
                    "feat": [parse_feature(f) for f in kids(ch, "eStructuralFeatures")],
                }
            elif kind == "EEnum":
                enums[cn] = {
                    "n": cn,
                    "x": xml_name(ch),
                    "lits": [[l.get("literal") or l.get("name"), l.get("name")]
                             for l in kids(ch, "eLiterals")],
                }
            elif kind == "EDataType":
                datatypes[cn] = {
                    "n": cn,
                    "x": xml_name(ch),
                    "ic": ch.get("instanceClassName"),
                }
        elif name == "eSubpackages":
            walk(ch, classes, enums, datatypes)


def main():
    out = sys.argv[2]
    classes, enums, datatypes = {}, {}, {}
    # argv[1] 是主 ecore，其余是附加 ecore（gautosar 等）
    for path in [sys.argv[1]] + sys.argv[3:]:
        walk(ET.parse(path).getroot(), classes, enums, datatypes)

    model = {
        "source": [p.split("/")[-1] for p in [sys.argv[1]] + sys.argv[3:]],
        "classes": classes,
        "enums": enums,
        "datatypes": datatypes,
    }
    with open(out, "w", encoding="utf-8") as fh:
        json.dump(model, fh, ensure_ascii=False, separators=(",", ":"))

    nfeat = sum(len(c["feat"]) for c in classes.values())
    print("classes   :", len(classes))
    print("features  :", nfeat)
    print("enums     :", len(enums))
    print("datatypes :", len(datatypes))
    print("带 xml.name 的类 :", sum(1 for c in classes.values() if c["x"]))
    print("out       :", out)


if __name__ == "__main__":
    main()