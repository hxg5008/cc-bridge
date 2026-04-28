#!/usr/bin/env bash
# 一键停止 cc-bridge (graceful: SIGTERM 后等 in-flight 排空)
set -euo pipefail
SERVICE_NAME="${CC_BRIDGE_SERVICE:-cc-bridge}"

[ "$(id -u)" = "0" ] || { echo "请用 sudo"; exit 1; }

if ! systemctl is-active --quiet "$SERVICE_NAME"; then
    echo "==> $SERVICE_NAME 已经停了"
    exit 0
fi

echo "==> 优雅停止 $SERVICE_NAME (最多等 30s 排空)"
systemctl stop "$SERVICE_NAME"
echo "==> ✓ 已停止"
