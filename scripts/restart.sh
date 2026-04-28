#!/usr/bin/env bash
# 一键重启 cc-bridge (graceful: 旧进程排空后再起新进程)
set -euo pipefail
SERVICE_NAME="${CC_BRIDGE_SERVICE:-cc-bridge}"

[ "$(id -u)" = "0" ] || { echo "请用 sudo"; exit 1; }

echo "==> 重启 $SERVICE_NAME"
systemctl restart "$SERVICE_NAME"

INSTALL_DIR="${CC_BRIDGE_DIR:-/opt/cc-bridge}"
PORT=5674
[ -f "$INSTALL_DIR/.env" ] && PORT=$(grep '^SERVER_PORT=' "$INSTALL_DIR/.env" | cut -d= -f2 || echo 5674)

for i in $(seq 1 30); do
    if curl -fsS "http://127.0.0.1:$PORT/readyz" >/dev/null 2>&1; then
        echo "==> ✓ 已就绪 (用时 ${i}s)"
        exit 0
    fi
    sleep 1
done

echo "!! 30s 内 /readyz 没通过"
journalctl -u "$SERVICE_NAME" --no-pager -n 30 || true
exit 1
