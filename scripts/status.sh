#!/usr/bin/env bash
# 一键查看 cc-bridge 运行状态
set -euo pipefail
SERVICE_NAME="${CC_BRIDGE_SERVICE:-cc-bridge}"
INSTALL_DIR="${CC_BRIDGE_DIR:-/opt/cc-bridge}"
PORT=5674
[ -f "$INSTALL_DIR/.env" ] && PORT=$(grep '^SERVER_PORT=' "$INSTALL_DIR/.env" | cut -d= -f2 || echo 5674)

echo "===================== cc-bridge =================="

# 1) 版本
if [ -f "$INSTALL_DIR/CURRENT_VERSION" ]; then
    echo "版本     : $(cat "$INSTALL_DIR/CURRENT_VERSION")"
elif [ -x "$INSTALL_DIR/cc-bridge" ]; then
    echo "版本     : $("$INSTALL_DIR/cc-bridge" --version 2>/dev/null || echo "(unknown)")"
fi

# 2) systemd 状态
echo -n "服务状态 : "
if systemctl is-active --quiet "$SERVICE_NAME"; then
    echo -e "\033[32mactive\033[0m"
else
    echo -e "\033[31m$(systemctl is-active "$SERVICE_NAME" 2>/dev/null || echo inactive)\033[0m"
fi

# 3) 健康端点
echo -n "/livez  : "
curl -fsS -m 2 "http://127.0.0.1:$PORT/livez" 2>/dev/null && echo "" || echo -e "\033[31mFAIL\033[0m"

echo -n "/readyz : "
curl -fsS -m 2 "http://127.0.0.1:$PORT/readyz" 2>/dev/null && echo "" || echo -e "\033[31mFAIL\033[0m"

# 4) Dashboard 摘要
if [ -f "$INSTALL_DIR/.env" ]; then
    ADMIN_PWD=$(grep '^ADMIN_PASSWORD=' "$INSTALL_DIR/.env" | cut -d= -f2 | tr -d '"' || echo "")
    if [ -n "$ADMIN_PWD" ]; then
        DASH=$(curl -fsS -m 3 -H "x-api-key: $ADMIN_PWD" \
            "http://127.0.0.1:$PORT/admin/dashboard" 2>/dev/null || echo "")
        if [ -n "$DASH" ]; then
            echo "账号统计 : $DASH"
        fi
    fi
fi

# 5) 关键 metrics
if curl -fsS -m 3 "http://127.0.0.1:$PORT/metrics" 2>/dev/null > /tmp/cc-metrics.$$; then
    echo "关键指标 :"
    grep -E '^ccbridge_(gateway_requests_total|gateway_errors_total|oauth_refresh_total\{platform="(claude|openai)",outcome="failure"|openai_failover_total|openai_account_filtered_by_limit_total|uptime_seconds)' \
        /tmp/cc-metrics.$$ | grep -v '^#' | sed 's/^/  /'
    rm -f /tmp/cc-metrics.$$
fi

# 6) 最近日志
echo
echo "----- 最近 10 条日志 (journalctl -u $SERVICE_NAME -f 看实时) -----"
journalctl -u "$SERVICE_NAME" --no-pager -n 10 2>/dev/null | tail -10 || echo "(无)"

echo "==================================================="
