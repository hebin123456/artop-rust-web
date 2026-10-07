-- artop-rust-web 初始化 schema
-- 账号/权限层 + Git-like ARXML 版本库层（L1-L6）
-- 注意：MySQL 保留字 blob / commit，故表名为 content_blob / commit_node
-- 由应用启动时自动执行（幂等，均带 IF NOT EXISTS）
-- 库本身由 scripts/setup_db.sh 创建

-- ===================== 账号 / 权限 =====================
CREATE TABLE IF NOT EXISTS app_user (
  id         BIGINT AUTO_INCREMENT PRIMARY KEY,
  username   VARCHAR(64)  NOT NULL,
  email      VARCHAR(160) NOT NULL,
  pass_hash  VARCHAR(255) NOT NULL,
  created_at DATETIME(3)  NOT NULL DEFAULT NOW(3),
  UNIQUE KEY uk_user_name (username)
) ENGINE=InnoDB;

CREATE TABLE IF NOT EXISTS repo (
  id         BIGINT AUTO_INCREMENT PRIMARY KEY,
  name       VARCHAR(128) NOT NULL,
  owner_id   BIGINT       NOT NULL,
  created_at DATETIME(3)  NOT NULL DEFAULT NOW(3),
  UNIQUE KEY uk_repo_name (name),
  CONSTRAINT fk_repo_owner FOREIGN KEY (owner_id) REFERENCES app_user(id)
) ENGINE=InnoDB;

-- 仓库级角色：reader < developer < reviewer < maintainer < owner
CREATE TABLE IF NOT EXISTS repo_member (
  repo_id  BIGINT NOT NULL,
  user_id  BIGINT NOT NULL,
  role     ENUM('reader','developer','reviewer','maintainer','owner') NOT NULL,
  added_at DATETIME(3) NOT NULL DEFAULT NOW(3),
  PRIMARY KEY (repo_id, user_id),
  CONSTRAINT fk_member_repo FOREIGN KEY (repo_id) REFERENCES repo(id) ON DELETE CASCADE,
  CONSTRAINT fk_member_user FOREIGN KEY (user_id) REFERENCES app_user(id)
) ENGINE=InnoDB;

-- ===================== 版本库 L1-L6 =====================

-- L1 内容寻址层
CREATE TABLE IF NOT EXISTS content_blob (
  repo_id BIGINT    NOT NULL,
  blob_id CHAR(40)  NOT NULL,
  cls     VARCHAR(96)  NOT NULL,
  sn      VARCHAR(255) NOT NULL,
  body    JSON      NOT NULL,
  PRIMARY KEY (repo_id, blob_id)
) ENGINE=InnoDB;

-- L2 当前视图层（HEAD 工作区）
CREATE TABLE IF NOT EXISTS element (
  repo_id     BIGINT        NOT NULL,
  element_uid VARCHAR(96)   NOT NULL,
  path        VARCHAR(1024) NOT NULL,
  cls         VARCHAR(96)   NOT NULL,
  blob_id     CHAR(40)      NOT NULL,
  PRIMARY KEY (repo_id, element_uid),
  KEY idx_elem_path (repo_id, path(200)),
  KEY idx_elem_cls  (repo_id, cls)
) ENGINE=InnoDB;

-- L5 引用图
CREATE TABLE IF NOT EXISTS ref_edge (
  repo_id BIGINT      NOT NULL,
  src_uid VARCHAR(96) NOT NULL,
  feat    VARCHAR(96) NOT NULL,
  tgt_uid VARCHAR(96) NOT NULL,
  KEY idx_edge_src (repo_id, src_uid),
  KEY idx_edge_tgt (repo_id, tgt_uid)
) ENGINE=InnoDB;

-- L3 提交 DAG
CREATE TABLE IF NOT EXISTS commit_node (
  repo_id     BIGINT       NOT NULL,
  commit_id   CHAR(40)     NOT NULL,
  parent_id   CHAR(40)     NULL,
  parent2_id  CHAR(40)     NULL,
  author_id   BIGINT       NOT NULL,
  msg         VARCHAR(1024) NOT NULL,
  created_at  DATETIME(3)  NOT NULL DEFAULT NOW(3),
  PRIMARY KEY (repo_id, commit_id),
  KEY idx_commit_parent (repo_id, parent_id)
) ENGINE=InnoDB;

-- 分支/标签指针（唯一可变状态，push 用 CAS 更新）
CREATE TABLE IF NOT EXISTS ref (
  repo_id    BIGINT       NOT NULL,
  name       VARCHAR(255) NOT NULL,
  kind       ENUM('branch','tag') NOT NULL DEFAULT 'branch',
  commit_id  CHAR(40)     NOT NULL,
  updated_at DATETIME(3)  NOT NULL DEFAULT NOW(3),
  PRIMARY KEY (repo_id, name)
) ENGINE=InnoDB;

-- L4 变更集（增量）
CREATE TABLE IF NOT EXISTS change_set (
  repo_id     BIGINT        NOT NULL,
  commit_id   CHAR(40)      NOT NULL,
  seq         INT UNSIGNED  NOT NULL,
  element_uid VARCHAR(96)   NOT NULL,
  path        VARCHAR(1024) NOT NULL,
  op          ENUM('A','M','D') NOT NULL,
  old_blob    CHAR(40)      NULL,
  new_blob    CHAR(40)      NULL,
  PRIMARY KEY (repo_id, commit_id, seq),
  KEY idx_cs_elem (repo_id, element_uid)
) ENGINE=InnoDB;

-- L6 评审 / 合入
CREATE TABLE IF NOT EXISTS review (
  id          BIGINT AUTO_INCREMENT PRIMARY KEY,
  repo_id     BIGINT       NOT NULL,
  src_branch  VARCHAR(255) NOT NULL,
  tgt_branch  VARCHAR(255) NOT NULL,
  head_commit CHAR(40)     NOT NULL,
  base_commit CHAR(40)     NULL,
  title       VARCHAR(255) NOT NULL,
  status      ENUM('open','approved','rejected','merged') NOT NULL DEFAULT 'open',
  author_id   BIGINT       NOT NULL,
  approver_id BIGINT       NULL,
  created_at  DATETIME(3)  NOT NULL DEFAULT NOW(3),
  decided_at  DATETIME(3)  NULL,
  KEY idx_review_repo (repo_id, status)
) ENGINE=InnoDB;

-- 审计日志（谁在什么时候做了什么，协作系统必需）
CREATE TABLE IF NOT EXISTS audit_log (
  id      BIGINT AUTO_INCREMENT PRIMARY KEY,
  repo_id BIGINT NULL,
  user_id BIGINT NULL,
  action  VARCHAR(64)  NOT NULL,
  detail  VARCHAR(512) NOT NULL,
  at      DATETIME(3)  NOT NULL DEFAULT NOW(3),
  KEY idx_audit_repo (repo_id, at)
) ENGINE=InnoDB;