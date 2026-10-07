#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
artop-rust-web 端到端 POC 验证脚本
=================================
用真实 AUTOSAR ARXML 语料（/tmp/dbpoc/corpus1.json）跑通完整系统：

  账号注册/登录(JWT) -> 仓库 -> 成员 RBAC(五级角色) -> 分支
  -> 推送(内容寻址 blob + 提交 DAG + 分支 CAS)
  -> 权限拒绝(非成员/低角色/自审/越权合入)
  -> 非快进推送冲突(409)
  -> 评审工作流(开单 -> 他人审批 -> maintainer 合入 -> 三方合并)
  -> 引用图影响分析 -> 审计日志 -> 实时协作(WebSocket + Redis pub/sub)

用法：
    python3 scripts/e2e_test.py [corpus.json] [BASE_URL]
依赖：仅标准库 + websockets（用于实时协作段，缺失则自动跳过该段）
"""

import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

BASE = os.environ.get("BASE", "http://127.0.0.1:8080")
CORPUS = os.environ.get("CORPUS", "/tmp/dbpoc/corpus1.json")
IMPORT_LIMIT = int(os.environ.get("IMPORT_LIMIT", "0"))  # 0 = 全量

PASS, FAIL, SKIP = [], [], []
CTX = {}


# ------------------------------------------------------------------ 工具
def call(method, path, body=None, token=None):
    """返回 (status, json)。"""
    data = json.dumps(body, ensure_ascii=False).encode() if body is not None else None
    req = urllib.request.Request(BASE + path, method=method, data=data)
    req.add_header("Content-Type", "application/json; charset=utf-8")
    if token:
        req.add_header("Authorization", "Bearer " + token)
    try:
        with urllib.request.urlopen(req, timeout=300) as r:
            txt = r.read().decode()
            return r.status, (json.loads(txt) if txt else None)
    except urllib.error.HTTPError as e:
        txt = e.read().decode()
        try:
            return e.code, json.loads(txt)
        except Exception:
            return e.code, {"raw": txt}


def check(name, ok, extra=""):
    (PASS if ok else FAIL).append(name)
    print("  [%s] %s%s" % ("PASS" if ok else "FAIL", name, ("  " + extra) if extra else ""), flush=True)
    return ok


def section(t):
    print("\n=== %s ===" % t, flush=True)


# ------------------------------------------------------------------ 语料
def load_corpus():
    print("[准备] 载入 ARXML 语料 %s" % CORPUS, flush=True)
    d = json.load(open(CORPUS))
    els, refs = d["elements"], d["refs"]
    if IMPORT_LIMIT:
        els = els[:IMPORT_LIMIT]
        keep = {e["id"] for e in els}
        refs = [r for r in refs if r["src"] in keep and r["tgt"] in keep]
    refmap = {}
    for r in refs:
        refmap.setdefault(r["src"], []).append({"feat": r["feat"], "tgt": r["tgt"]})
    print("       元素 %d，引用 %d" % (len(els), len(refs)), flush=True)
    return els, refs, refmap


def change_of(e, refmap, op="A", rev=0):
    return {
        "uid": e["id"],
        "path": e["path"],
        "cls": e["cls"],
        "op": op,
        "sn": e.get("sn"),
        "attrs": {"name": e.get("sn") or "", "rev": rev},
        "refs_out": refmap.get(e["id"], []),
    }


# ------------------------------------------------------------------ 主流程
def main():
    suffix = str(int(time.time()))
    els, refs, refmap = load_corpus()

    section("1. 账号：注册 / 登录 / JWT")
    users = {
        "alice": ("owner", None),
        "bob": ("developer", None),
        "carol": ("reviewer", None),
        "dave": ("maintainer", None),
        "eve": ("outsider", None),
    }
    for name in users:
        u = "%s_%s" % (name, suffix)
        st, r = call("POST", "/api/register", {"username": u, "email": u + "@x.io", "password": "pw123456"})
        if st != 200:
            check("注册 %s" % name, False, str(r))
            return
        st, r = call("POST", "/api/login", {"username": u, "password": "pw123456"})
        users[name] = (r["user"]["id"], r["token"])
    check("注册并登录 5 个用户", all(users[n][1] for n in users))

    st, r = call("POST", "/api/login", {"username": "alice_" + suffix, "password": "wrong"})
    check("错误口令被拒绝(401)", st == 401, "got %d" % st)

    st, r = call("GET", "/api/me", token=users["alice"][1])
    check("JWT 鉴权 /api/me", st == 200 and r["id"] == users["alice"][0])

    st, r = call("GET", "/api/repos")
    check("无 token 访问被拒(401)", st == 401, "got %d" % st)

    section("2. 仓库 + 成员 RBAC")
    alice_t, bob_t, carol_t, dave_t, eve_t = (users[n][1] for n in ["alice", "bob", "carol", "dave", "eve"])
    st, r = call("POST", "/api/repos", {"name": "arxml-poc-" + suffix}, token=alice_t)
    if st != 200:
        check("创建仓库", False, str(r))
        return
    repo = r["id"]
    CTX["repo"] = repo
    check("alice 创建仓库(#%d)，自动成为 owner" % repo, r["role"] == "owner")

    for who, role in [("bob", "developer"), ("carol", "reviewer"), ("dave", "maintainer")]:
        uname = who + "_" + suffix
        st, r = call("POST", "/api/repos/%d/members" % repo, {"username": uname, "role": role}, token=alice_t)
        check("alice 添加 %s=%s" % (who, role), st == 200, str(r) if st != 200 else "")

    st, r = call("POST", "/api/repos/%d/members" % repo, {"username": "bob_" + suffix, "role": "reader"}, token=bob_t)
    check("developer 无权管理成员(403)", st == 403, "got %d" % st)

    st, r = call("GET", "/api/repos/%d/branches" % repo, token=eve_t)
    check("非成员读仓库被拒(403)", st == 403, "got %d" % st)

    section("3. 推送：内容寻址 + 提交 DAG + 分支 CAS")
    t0 = time.time()
    changes = [change_of(e, refmap, "A") for e in els]
    st, r = call("POST", "/api/repos/%d/push" % repo,
                 {"branch": "main", "message": "import real ARXML model", "changes": changes}, token=bob_t)
    dt = time.time() - t0
    if st != 200:
        check("首次推送全量模型", False, str(r)[:300])
        return
    c1 = r["commit_id"]
    CTX["c1"] = c1
    check("developer 首次推送 %d 个元素 -> 建 main 分支" % len(changes), r["applied"] == "branch-created",
          "commit=%s  %.1fs  (%.0f elem/s)" % (c1[:8], dt, len(changes) / max(dt, 1e-6)))

    # 增量提交：改 200 个 + 新增 20 个
    mods = [change_of(e, refmap, "M", rev=1) for e in els[:200]]
    newels = [{"id": "poc-new-%s-%d" % (suffix, i), "cls": "SW-COMPONENT", "sn": "New_%d" % i,
               "path": "/POC/New_%d" % i} for i in range(20)]
    changes2 = mods + [change_of(e, refmap, "A") for e in newels]
    st, r = call("POST", "/api/repos/%d/push" % repo,
                 {"branch": "main", "base_commit": c1, "message": "tune 200 + add 20", "changes": changes2},
                 token=bob_t)
    check("增量推送(200 改 + 20 增)", st == 200 and r["applied"] == "fast-forward", str(r)[:200])
    c2 = r["commit_id"]
    CTX["c2"] = c2

    st, r = call("POST", "/api/repos/%d/push" % repo,
                 {"branch": "main", "base_commit": c1, "message": "stale", "changes": changes2}, token=bob_t)
    check("非快进推送被拒(409)", st == 409, "got %d" % st)

    section("4. 分支 + 评审合入（含三方合并）")
    st, r = call("POST", "/api/repos/%d/branches" % repo, {"name": "feature/x", "from": "main"}, token=alice_t)
    check("maintainer/owner 建分支 feature/x", st == 200, str(r) if st != 200 else "")

    st, r = call("POST", "/api/repos/%d/branches" % repo, {"name": "feature/y", "from": "main"}, token=bob_t)
    check("developer 无权建分支(403)", st == 403, "got %d" % st)

    st, r = call("GET", "/api/repos/%d/branches" % repo, token=bob_t)
    fhead = next(b["head"] for b in r["branches"] if b["name"] == "feature/x")

    # feature 上继续开发：再改 50 个元素
    st, r = call("POST", "/api/repos/%d/push" % repo,
                 {"branch": "feature/x", "base_commit": fhead, "message": "feature tweaks",
                  "changes": [change_of(e, refmap, "M", rev=7) for e in els[200:250]]}, token=bob_t)
    check("developer 在 feature/x 上提交", st == 200, str(r)[:200])
    fhead2 = r["commit_id"]

    # 同时让 main 前进，制造分叉（验证三方合并基点）
    st, r = call("POST", "/api/repos/%d/push" % repo,
                 {"branch": "main", "base_commit": c2, "message": "hotfix on main",
                  "changes": [change_of(e, refmap, "M", rev=9) for e in els[300:310]]}, token=bob_t)
    check("main 独立前进，形成分叉", st == 200, str(r)[:200])

    st, r = call("POST", "/api/repos/%d/reviews" % repo,
                 {"src_branch": "feature/x", "tgt_branch": "main", "title": "合入 feature/x"}, token=carol_t)
    if st != 200:
        check("reviewer 开评审", False, str(r)[:200])
        return
    rid = r["review"]["id"]
    check("reviewer(carol) 开评审 #%d" % rid, r["review"]["status"] == "open")

    st, r = call("GET", "/api/reviews/%d/merge-check" % rid, token=dave_t)
    check("合入预演：识别分叉且无冲突", st == 200 and r["mergeable"] and r["base"] is not None,
          "base=%s conflicts=%d" % (str(r.get("base"))[:8], r["conflict_count"]))

    st, r = call("POST", "/api/reviews/%d/decide" % rid, {"approve": True}, token=carol_t)
    check("作者自审被拒(403，职责分离)", st == 403, "got %d" % st)

    st, r = call("POST", "/api/reviews/%d/decide" % rid, {"approve": True}, token=bob_t)
    check("developer 审批权限不足(403)", st == 403, "got %d" % st)

    st, r = call("POST", "/api/reviews/%d/decide" % rid, {"approve": True}, token=alice_t)
    check("owner(alice) 审批通过", st == 200 and r["review"]["status"] == "approved", str(r)[:150])

    st, r = call("POST", "/api/reviews/%d/merge" % rid, token=bob_t)
    check("developer 合入权限不足(403)", st == 403, "got %d" % st)

    st, r = call("POST", "/api/reviews/%d/merge" % rid, token=dave_t)
    check("maintainer(dave) 合入成功", st == 200 and len(r["merge"]["merge_commit"]) == 40,
          "merge=%s" % (r.get("merge", {}).get("merge_commit", "")[:8] if st == 200 else str(r)[:150]))
    merge_c = r["merge"]["merge_commit"] if st == 200 else None
    CTX["merge"] = merge_c

    st, r = call("GET", "/api/repos/%d/log?branch=main" % repo, token=carol_t)
    top = r["commits"][0]
    check("合入后 main 头部 = merge commit，且是双亲提交", top["commit_id"] == merge_c and top["parent2"] is not None,
          "parent=%s parent2=%s" % (str(top["parent"])[:8], str(top["parent2"])[:8]))

    st, r = call("GET", "/api/reviews/%d" % rid, token=carol_t)
    check("评审状态流转 merged", r["review"]["status"] == "merged")

    section("5. 历史 / 差异 / 影响分析 / 审计")
    st, r = call("GET", "/api/repos/%d/log?branch=main" % repo, token=carol_t)
    check("提交历史可追溯(%d 条)" % len(r["commits"]), len(r["commits"]) == 4)

    st, r = call("GET", "/api/repos/%d/diff?from=%s&to=%s" % (repo, c1, c2), token=carol_t)
    check("diff 统计正确(200 改/20 增)",
          r["summary"]["modified"] == 200 and r["summary"]["added"] == 20, json.dumps(r["summary"]))

    if refs:
        target = refs[0]["tgt"]
        st, r = call("GET", "/api/repos/%d/impact?uid=%s&depth=4" % (repo, urllib.parse.quote(target)),
                     token=carol_t)
        check("引用图影响分析：目标 %s 反向可达 %d 个元素" % (target[:16], r["total"]),
              st == 200 and r["total"] >= 1)
    else:
        SKIP.append("影响分析（语料无引用）")

    st, r = call("GET", "/api/repos/%d/audit" % repo, token=alice_t)
    acts = {a["action"] for a in r["audit"]}
    check("审计日志覆盖 push/review/merge", {"push", "review.open", "review.decide", "review.merge"} <= acts,
          ",".join(sorted(acts)))

    section("5b. 元素编辑器（ARTOP Edit：内容树 / 详情 / 搜索 / 编辑提交）")
    # 内容树根：AUTOSAR 包应既是元素、又可继续展开
    st, r = call("GET", "/api/repos/%d/tree?parent=" % repo, token=carol_t)
    root = r.get("nodes", []) if st == 200 else []
    auto = next((n for n in root if n["name"] == "AUTOSAR"), None)
    check("内容树根节点返回，AUTOSAR 包既是元素又可展开",
          st == 200 and auto is not None and auto["is_element"] and auto["has_children"],
          "root nodes=%d" % len(root))

    st, r = call("GET", "/api/repos/%d/tree?parent=%s" % (repo, urllib.parse.quote("/AUTOSAR")), token=carol_t)
    check("内容树懒加载 /AUTOSAR 子节点", st == 200 and len(r.get("nodes", [])) > 0,
          "children=%d" % len(r.get("nodes", [])))

    # 元素详情：带属性 + 出/入向引用（引用需回填对端可读名）
    src_uid = refs[0]["src"] if refs else els[0]["id"]
    st, r = call("GET", "/api/repos/%d/elements/%s" % (repo, urllib.parse.quote(src_uid)), token=carol_t)
    el = r.get("element", {}) if st == 200 else {}
    check("元素详情返回属性与出向引用",
          st == 200 and el.get("uid") == src_uid and len(el.get("refs_out", [])) >= 1,
          "cls=%s out=%d in=%d" % (el.get("cls"), len(el.get("refs_out", [])), len(el.get("refs_in", []))))

    # 搜索（编辑器"查找元素"）
    probe = els[3]
    st, r = call("GET", "/api/repos/%d/elements?q=%s&limit=20" % (repo, urllib.parse.quote(probe["sn"])), token=carol_t)
    hits = r.get("elements", []) if st == 200 else []
    check("按名称搜索命中元素", st == 200 and any(e["uid"] == probe["id"] for e in hits),
          "%s -> %d 条" % (probe["sn"], len(hits)))

    # 类分布（编辑器分类统计）
    st, r = call("GET", "/api/repos/%d/classes" % repo, token=carol_t)
    total = sum(c["count"] for c in r.get("classes", [])) if st == 200 else -1
    check("类分布统计等于当前视图元素数", st == 200 and total == len(els) + 20,
          "sum=%d vs %d" % (total, len(els) + 20))

    # 越权：非成员读内容树被拒
    st, r = call("GET", "/api/repos/%d/tree?parent=" % repo, token=eve_t)
    check("非成员读内容树被拒(403)", st == 403, "got %d" % st)

    # 编辑保存：GET 详情 -> 改属性 -> push -> 再 GET 读回（编辑器核心闭环）
    st, r = call("GET", "/api/repos/%d/elements/%s" % (repo, urllib.parse.quote(probe["id"])), token=bob_t)
    det = r["element"]
    new_attrs = dict(det.get("attrs") or {})
    new_attrs["edited_by"] = "editor"
    new_attrs["rev"] = 4242
    edit_change = {"uid": det["uid"], "path": det["path"], "cls": det["cls"], "op": "M",
                   "sn": det.get("sn"), "attrs": new_attrs,
                   "refs_out": [{"feat": x["feat"], "tgt": x["tgt"]} for x in det.get("refs_out", [])]}
    st, r = call("POST", "/api/repos/%d/push" % repo,
                 {"branch": "editor/scratch", "message": "editor: edit %s" % det.get("sn"),
                  "changes": [edit_change]}, token=bob_t)
    check("编辑器保存：首次提交新建 editor/scratch 分支",
          st == 200 and r["applied"] == "branch-created", str(r)[:160])
    esc_head = r["commit_id"] if st == 200 else None

    st, r = call("GET", "/api/repos/%d/elements/%s" % (repo, urllib.parse.quote(probe["id"])), token=bob_t)
    at = r["element"]["attrs"] if st == 200 else {}
    check("编辑器保存后属性可读回", at.get("edited_by") == "editor" and at.get("rev") == 4242,
          json.dumps(at, ensure_ascii=False))

    if esc_head:
        new_attrs2 = dict(new_attrs)
        new_attrs2["rev"] = 4243
        edit_change["attrs"] = new_attrs2
        st, r = call("POST", "/api/repos/%d/push" % repo,
                     {"branch": "editor/scratch", "base_commit": esc_head,
                      "message": "editor: tweak again", "changes": [edit_change]}, token=bob_t)
        check("编辑器再次保存走快进提交", st == 200 and r["applied"] == "fast-forward", str(r)[:160])

    # 编辑器新建元素
    new_el = {"uid": "editor-new-%s" % suffix, "path": "/POC/EditorMade", "cls": "SW-COMPONENT",
              "op": "A", "sn": "EditorMade", "attrs": {"name": "EditorMade"}, "refs_out": []}
    st, r = call("GET", "/api/repos/%d/branches" % repo, token=bob_t)
    esh = next((b["head"] for b in r["branches"] if b["name"] == "editor/scratch"), None)
    st, r = call("POST", "/api/repos/%d/push" % repo,
                 {"branch": "editor/scratch", "base_commit": esh, "message": "editor: create element",
                  "changes": [new_el]}, token=bob_t)
    check("编辑器新建元素提交成功", st == 200, str(r)[:160])
    st, r = call("GET", "/api/repos/%d/elements/%s" % (repo, urllib.parse.quote(new_el["uid"])), token=bob_t)
    check("新建元素可被读取", st == 200 and r["element"]["path"] == "/POC/EditorMade", str(r)[:160])

    section("6. 实时协作（WebSocket + Redis pub/sub）")
    try:
        import asyncio
        import websockets

        async def ws_flow():
            ws_url = BASE.replace("http://", "ws://").replace("https://", "wss://")
            a_uri = "%s/ws?repo_id=%d&token=%s" % (ws_url, repo, users["bob"][1])
            b_uri = "%s/ws?repo_id=%d&token=%s" % (ws_url, repo, users["carol"][1])

            async with websockets.connect(a_uri) as wa, websockets.connect(b_uri) as wb:
                ha = json.loads(await asyncio.wait_for(wa.recv(), 5))
                hb = json.loads(await asyncio.wait_for(wb.recv(), 5))
                check("两端 WS 建连并收到 hello", ha["type"] == "hello" and hb["type"] == "hello")

                # A 发协作编辑指令，B 应收到；A 自己不应收到（origin 去重）
                await wa.send(json.dumps({"type": "edit", "action": "setAttr",
                                          "payload": {"uid": els[0]["id"], "rev": 42}}))
                got_b = json.loads(await asyncio.wait_for(wb.recv(), 5))
                check("B 实时收到 A 的编辑指令", got_b.get("type") == "edit" and got_b.get("by") == "bob_" + suffix,
                      json.dumps(got_b)[:120])

                try:
                    echo = await asyncio.wait_for(wa.recv(), 1.2)
                    check("A 不回环收到自己的消息", False, echo[:80])
                except asyncio.TimeoutError:
                    check("A 不回环收到自己的消息", True)

                # 走 HTTP 推一次，B 应收到 push 广播
                call("POST", "/api/repos/%d/push" % repo,
                     {"branch": "main", "message": "realtime probe",
                      "base_commit": merge_c,
                      "changes": [change_of(els[0], refmap, "M", rev=99)]}, token=bob_t)
                while True:
                    ev = json.loads(await asyncio.wait_for(wb.recv(), 5))
                    if ev.get("type") == "push":
                        break
                check("HTTP 推送通过 Redis 广播到在线端", ev["branch"] == "main")

                # 非成员连 WS 应被拒
                try:
                    async with websockets.connect("%s/ws?repo_id=%d&token=%s" % (ws_url, repo, users["eve"][1])) as _:
                        check("非成员 WS 连接被拒", False)
                except Exception:
                    check("非成员 WS 连接被拒", True)

        asyncio.get_event_loop().run_until_complete(ws_flow())
    except ImportError:
        SKIP.append("实时协作（未安装 websockets）")
        print("  [SKIP] 未安装 websockets，跳过实时协作段", flush=True)

    section("结果")
    print("  PASS %d / FAIL %d / SKIP %d" % (len(PASS), len(FAIL), len(SKIP)))
    if FAIL:
        print("  失败项：")
        for f in FAIL:
            print("    - " + f)
    return 0 if not FAIL else 1


if __name__ == "__main__":
    if len(sys.argv) > 1:
        CORPUS = sys.argv[1]
    if len(sys.argv) > 2:
        BASE = sys.argv[2]
    if not os.path.exists(CORPUS):
        print("语料不存在：%s" % CORPUS)
        sys.exit(2)
    sys.exit(main())