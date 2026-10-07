# artop-rust-web

Git-like 协同 ARXML 模型仓库的**纯 Rust 全栈 POC**。

回答的核心问题：**「协同 + 账号权限」是不是必须上 Spring？**
—— **不需要。** `axum + sqlx + Redis` 一套 Rust 就能把账号、RBAC、版本库、评审、实时协作全部写完，
形态与 Spring 方案等价，且没有 JVM 与多语言栈的额外开销。

---

## 1. 一个进程里提供的能力

| 层 | 能力 | 实现 |
|---|---|---|
| 账号 | 注册 / 登录 / JWT(HS256) / Argon2id 口令哈希 | [auth.rs](src/auth.rs) |
| 权限 | 仓库级五级 RBAC：`reader < developer < reviewer < maintainer < owner` | [rbac.rs](src/rbac.rs) |
| 版本库 | 内容寻址 blob + 提交 DAG + 分支指针 CAS + 三方合并 | [vcs.rs](src/vcs.rs) |
| 评审 | 开 MR → 他人审批（禁止自审）→ maintainer 合入 | [review.rs](src/review.rs) |
| 实时协作 | WebSocket + Redis pub/sub 扇出，多实例天然互通 | [realtime.rs](src/realtime.rs) |
| HTTP API | 全部 REST 接口 + 静态前端托管 | [api.rs](src/api.rs) |
| 前端 | 单文件验证页（分支/推送/评审/成员/影响分析/实时事件） | [static/index.html](static/index.html) |

### 角色 → 动作矩阵

| 动作 | 最低角色 |
|---|---|
| 读仓库 / 看历史 / 影响分析 | reader |
| 提交 / 推送 / 开评审 | developer |
| 审批通过 / 驳回 | reviewer |
| 合入 / 建分支 / 管成员 | maintainer |
| 仓库所有权 | owner（创建者） |

## 2. 三库分工

- **MySQL**：主库。账号、权限、提交 DAG、变更集、引用图、评审、审计。单事务保证「要么全成要么全败」。
- **Redis**：实时层。`SET NX PX` 分布式推送锁、分支头缓存、pub/sub 事件总线。
- MongoDB 作为可选「内容库」用于超大 blob 归档（本 POC 未启用）。

## 3. 关键设计

**内容寻址**：元素规范化序列化后取 SHA-1 作为 `blob_id`，天然去重；
提交 id 由 `仓库|分支|父|作者|消息|变更指纹` 派生。

**推送 = 提交 + 推进指针，全程一个事务**：
1. Redis `SET NX PX` 拿分支推送锁（同分支串行化，进程崩溃自动过期）
2. 事务内 `SELECT ... FOR UPDATE` 读分支头，做**快进检查**
3. 写 blob / element / ref_edge / change_set / commit_node
4. `UPDATE ref ... WHERE commit_id = <旧头>` 做 **CAS**，`rows_affected != 1` 即冲突回滚

**三方合并**：从两侧向上求 merge base，各自算 `base..head` 的净变更；
同一元素两侧新 blob 不同即判冲突，有冲突直接拒绝合入。

**实时协作**：每个 WS 连接在服务端建独立 Redis 订阅，事件带 `origin`(连接 id) 去重防回环。
服务端先订阅成功再发 `hello`，保证客户端收到 hello 即不漏事件。

## 4. 运行

```bash
# 1) 准备 MySQL 库与账号（一次性）
bash scripts/setup_db.sh

# 2) 起 Redis（默认 redis://127.0.0.1:6379）

# 3) 运行（启动时自动建表，幂等）
cargo run
# 打开 http://127.0.0.1:8080/
```

可用环境变量：`BIND`(默认 `0.0.0.0:8080`)、`DATABASE_URL`、`REDIS_URL`、`JWT_SECRET`。

## 5. 端到端验证

```bash
python3 scripts/e2e_test.py [corpus.json] [http://127.0.0.1:8080]
```

用**真实 AUTOSAR 4.4.8 标准库 ARXML**（15,264 个元素 / 3,209 条引用）跑通全链路。
实测结果 **35 项检查全部 PASS**：

```
=== 1. 账号：注册 / 登录 / JWT ===          4 PASS
=== 2. 仓库 + 成员 RBAC ===                 6 PASS
=== 3. 推送：内容寻址 + 提交 DAG + CAS ===  3 PASS   (15,264 元素首次全量推送 8.3s ≈ 1,830 elem/s)
=== 4. 分支 + 评审合入（含三方合并）===      12 PASS
=== 5. 历史 / 差异 / 影响分析 / 审计 ===     4 PASS
=== 6. 实时协作（WebSocket + Redis）===      5 PASS
PASS 35 / FAIL 0 / SKIP 0
```

覆盖的负向用例：错误口令 401、无 token 401、非成员 403、低角色越权 403、
**作者自审 403**、developer 越权合入 403、**非快进推送 409**、非成员 WS 拒连。

## 6. 目录

```
src/
  main.rs      启动 + 路由装配（自动建表）
  config.rs    环境变量配置
  state.rs     共享状态（MySQL 池 + Redis + 配置）
  error.rs     统一错误 → HTTP 状态码
  auth.rs      Argon2id + JWT + 鉴权提取器
  rbac.rs      五级角色与 require()
  vcs.rs       推送/分支/diff/影响分析/合并基点/三方合并
  review.rs    评审工作流 + 提交历史
  realtime.rs  WebSocket ↔ Redis pub/sub
  api.rs       REST 路由
migrations/0001_init.sql   schema
scripts/e2e_test.py        端到端验证
scripts/setup_db.sh        数据库初始化
static/index.html          验证前端
```

## 7. 与 Spring 方案的对照

| 关注点 | Spring 方案 | 本项目（纯 Rust） |
|---|---|---|
| Web / 路由 | Spring MVC | axum |
| 持久化 | Spring Data JPA / MyBatis | sqlx（编译期校验 SQL） |
| 鉴权 | Spring Security + JWT | jsonwebtoken + 自定义提取器 |
| 实时 | STOMP over WebSocket | axum ws + Redis pub/sub |
| 事务 | `@Transactional` | `sqlx::Transaction` 显式控制 |
| 部署 | JVM + 应用服务器 | 单个静态二进制 |

结论：账号权限与协同**不构成选 Spring 的理由**；纯 Rust 栈可以统一后端与前端建模（未来 Rust→Wasm），
减少一套语言与运行时。