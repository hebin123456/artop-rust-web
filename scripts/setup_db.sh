#!/usr/bin/env bash
# 初始化 artop-rust-web 所需的 MySQL 库与账号（幂等，需 root 权限）
# 用法：bash scripts/setup_db.sh [db_name] [db_user] [db_pass]
set -euo pipefail

DB="${1:-artop_web}"
USER="${2:-poc}"
PASS="${3:-pocpass}"

echo ">> 创建库 ${DB} 与账号 ${USER}"
mysql -uroot <<SQL
CREATE DATABASE IF NOT EXISTS \`${DB}\` CHARACTER SET utf8mb4 COLLATE utf8mb4_bin;
CREATE USER IF NOT EXISTS '${USER}'@'127.0.0.1' IDENTIFIED BY '${PASS}';
CREATE USER IF NOT EXISTS '${USER}'@'localhost' IDENTIFIED BY '${PASS}';
GRANT ALL PRIVILEGES ON \`${DB}\`.* TO '${USER}'@'127.0.0.1';
GRANT ALL PRIVILEGES ON \`${DB}\`.* TO '${USER}'@'localhost';
FLUSH PRIVILEGES;
SQL

echo ">> 完成。启动应用时会自动建表，DATABASE_URL 示例："
echo "   mysql://${USER}:${PASS}@127.0.0.1:3306/${DB}"