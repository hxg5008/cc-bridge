#!/usr/bin/env bash
# 一键启动 cc-bridge
set -euo pipefail
SERVICE_NAME="${CC_BRIDGE_SERVICE:-cc-bridge}"

[ "$(id -u)" = "0" ] || { echo "请用 sudo"; exit 1; }

if systemctl is-active --quiet "$SERVICE_NAME"; then
    echo "==> $SERVICE_NAME 已经在跑"
    exit 0
fi

echo "==> 启动 $SERVICE_NAME"
systemctl start "$SERVICE_NAME"

INSTALL_DIR="${CC_BRIDGE_DIR:-/opt/cc-bridge}"
PORT=5674
[ -f "$INSTALL_DIR/.env" ] && PORT=$(grep '^SERVER_PORT=' "$INSTALL_DIR/.env" | cut -d= -f2 || echo 5674)

for i in $(seq 1 20); do
    if curl -fsS "http://127.0.0.1:$PORT/readyz" >/dev/null 2>&1; then
        echo "==> ✓ 已就绪 (用时 ${i}s, http://127.0.0.1:$PORT)"
        exit 0
    fi
    sleep 1
done

echo "!! 20s 内 /readyz 没通过,看 journalctl -u $SERVICE_NAME -f"
exit 1
