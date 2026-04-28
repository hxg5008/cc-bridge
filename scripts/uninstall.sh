#!/usr/bin/env bash
# cc-bridge 卸载脚本
#
# 用法:
#   sudo ./uninstall.sh           # 保留数据目录, 仅删 binary + service
#   sudo ./uninstall.sh --purge   # 全删 (含数据库 + 配置 + 用户)

set -euo pipefail

INSTALL_DIR="${CC_BRIDGE_DIR:-/opt/cc-bridge}"
SERVICE_USER="${CC_BRIDGE_USER:-ccbridge}"
SERVICE_NAME="cc-bridge"
PURGE=0

[ "${1:-}" = "--purge" ] && PURGE=1
[ "$(id -u)" = "0" ] || { echo "请用 sudo"; exit 1; }

echo "==> 停止 + 禁用 service"
systemctl stop "$SERVICE_NAME" 2>/dev/null || true
systemctl disable "$SERVICE_NAME" 2>/dev/null || true
rm -f "/etc/systemd/system/${SERVICE_NAME}.service"
systemctl daemon-reload

if [ "$PURGE" = "1" ]; then
    echo "==> --purge: 删除全部数据"
    rm -rf "$INSTALL_DIR"
    if id "$SERVICE_USER" >/dev/null 2>&1; then
        userdel "$SERVICE_USER" 2>/dev/null || true
    fi
    echo "==> ✓ 全部清除"
else
    echo "==> 删除 binary, 保留 $INSTALL_DIR/data 和 .env"
    rm -f "$INSTALL_DIR/cc-bridge"
    rm -f "$INSTALL_DIR/CURRENT_VERSION"
    echo "==> ✓ 完成 (重装直接跑 install.sh)"
fi
