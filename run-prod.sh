#!/usr/bin/env bash
# 生产启动脚本。
#
# 为什么需要它：UPS_JWT_SECRET / UPS_ADMIN_PASSWORD 只从环境变量读（程序不加载 .env），
# 手工 export 一旦漏掉就会静默退回默认密钥，只在启动日志里留一条 WARN。
# 把密钥放进 .env.prod（已 gitignore）由脚本统一导出，比每次手敲可靠。
set -euo pipefail
cd "$(dirname "$0")"

if [ ! -f .env.prod ]; then
    echo "缺少 .env.prod：先复制 .env.prod.example 并填入随机密钥" >&2
    exit 1
fi

if [ ! -x ./target/release/mdvr_ups.exe ]; then
    echo "找不到 ./target/release/mdvr_ups.exe，先执行 cargo build --release" >&2
    exit 1
fi

# set -a 让 source 进来的变量自动 export
set -a
# shellcheck disable=SC1091
. ./.env.prod
set +a

exec ./target/release/mdvr_ups.exe
