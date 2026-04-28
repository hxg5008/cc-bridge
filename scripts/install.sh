#!/usr/bin/env bash
# cc-bridge 一键首装脚本
#
# 在一台干净的 Ubuntu/Debian 服务器上跑这个,会:
#   1. 创建 ccbridge 系统用户 + /opt/cc-bridge 目录
#   2. 从 GitHub Releases 下载最新 binary (优先 latest tag)
#   3. 写默认 .env 配置 (ADMIN_PASSWORD 自动生成强密码)
#   4. 安装 systemd unit, 开机自启
#   5. 启动服务并等待 /readyz 就绪
#
# 用法:
#   sudo ./install.sh                      # 装最新版
#   sudo ./install.sh v1.8.5               # 装指定版本
#   sudo GH_TOKEN=ghp_xxx ./install.sh     # 私有仓库需要 token
#
# 卸载: ./uninstall.sh (附带的)

set -euo pipefail

# ---- 配置 (按需改这些常量) ----
REPO="${CC_BRIDGE_REPO:-YOUR_GH_USER/cc-bridge}"
INSTALL_DIR="${CC_BRIDGE_DIR:-/opt/cc-bridge}"
SERVICE_USER="${CC_BRIDGE_USER:-ccbridge}"
SERVICE_NAME="cc-bridge"
DEFAULT_PORT="${CC_BRIDGE_PORT:-5674}"

# ---- 工具函数 ----
say()  { printf '\033[1;32m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[1;33m!!  %s\033[0m\n' "$*" >&2; }
die()  { printf '\033[1;31mxx  %s\033[0m\n' "$*" >&2; exit 1; }

# ---- 0) 前置检查 ----
[ "$(id -u)" = "0" ] || die "请用 root 或 sudo 跑"
[ -f /etc/os-release ] || die "/etc/os-release 不存在,无法识别系统"
. /etc/os-release
case "${ID_LIKE:-$ID}" in
    *debian*|*ubuntu*) ;;
    *) warn "未在 Ubuntu/Debian 测试过 ($ID), 继续但可能踩坑" ;;
esac

ARCH=$(uname -m)
case "$ARCH" in
    x86_64|amd64)  ARTIFACT="cc-bridge-linux-amd64" ;;
    aarch64|arm64) ARTIFACT="cc-bridge-linux-arm64" ;;
    *)             die "不支持的架构: $ARCH (只支持 x86_64 / aarch64)" ;;
esac

TAG="${1:-latest}"
say "目标: $REPO @ $TAG, 架构 $ARCH ($ARTIFACT)"

# ---- 1) 装系统依赖 ----
say "装系统依赖 (curl, ca-certificates)"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq curl ca-certificates tar coreutils >/dev/null

# ---- 2) 创建用户 + 目录 ----
if ! id "$SERVICE_USER" >/dev/null 2>&1; then
    say "创建系统用户 $SERVICE_USER"
    useradd --system --home "$INSTALL_DIR" --shell /usr/sbin/nologin "$SERVICE_USER"
fi
mkdir -p "$INSTALL_DIR"/{data,backups}

# ---- 3) 下载 binary ----
say "查询版本信息"
AUTH=()
[ -n "${GH_TOKEN:-}" ] && AUTH=(-H "Authorization: Bearer $GH_TOKEN")

if [ "$TAG" = "latest" ]; then
    TAG=$(curl -fsSL "${AUTH[@]}" \
        "https://api.github.com/repos/$REPO/releases/latest" \
        | grep -oP '"tag_name":\s*"\K[^"]+' || true)
    [ -n "$TAG" ] || die "拉取 latest tag 失败 (私有仓库需要 GH_TOKEN=ghp_xxx)"
fi
say "  即将安装 $TAG"

DL_URL="https://github.com/$REPO/releases/download/$TAG/${ARTIFACT}.tar.gz"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

say "下载 $DL_URL"
curl -fL "${AUTH[@]}" "$DL_URL" -o "$TMP/cc-bridge.tar.gz" \
    || die "下载失败,检查 tag 是否存在 / 私有仓库 token 是否正确"

# 校验 sha256 (release.yml 暂未生成 .sha256 → 跳过, 留 placeholder)
if curl -fsL "${AUTH[@]}" "${DL_URL}.sha256" -o "$TMP/cc-bridge.tar.gz.sha256" 2>/dev/null; then
    say "校验 SHA256"
    (cd "$TMP" && sha256sum -c cc-bridge.tar.gz.sha256) || die "SHA256 校验失败"
fi

tar xzf "$TMP/cc-bridge.tar.gz" -C "$TMP"
[ -f "$TMP/$ARTIFACT" ] || die "tarball 内未找到 $ARTIFACT"

# ---- 4) 备份旧 binary (升级时) ----
if [ -f "$INSTALL_DIR/cc-bridge" ]; then
    BAK_DIR="$INSTALL_DIR/backups/$(date +%Y%m%d-%H%M%S)"
    mkdir -p "$BAK_DIR"
    cp "$INSTALL_DIR/cc-bridge" "$BAK_DIR/" || true
    [ -f "$INSTALL_DIR/data/claude-code-gateway.db" ] \
        && cp "$INSTALL_DIR/data/claude-code-gateway.db" "$BAK_DIR/" || true
    say "已备份旧版本到 $BAK_DIR"
fi

# ---- 5) 装 binary ----
install -m 0755 -o "$SERVICE_USER" -g "$SERVICE_USER" "$TMP/$ARTIFACT" "$INSTALL_DIR/cc-bridge"
echo "$TAG" > "$INSTALL_DIR/CURRENT_VERSION"
chown "$SERVICE_USER:$SERVICE_USER" "$INSTALL_DIR/CURRENT_VERSION"

# ---- 6) .env 默认配置 (仅首次) ----
if [ ! -f "$INSTALL_DIR/.env" ]; then
    say "生成默认 .env (含随机 ADMIN_PASSWORD)"
    ADMIN_PWD=$(head -c 18 /dev/urandom | base64 | tr -d '+/=' | head -c 24)
    cat > "$INSTALL_DIR/.env" <<EOF
# cc-bridge 配置 (首次安装自动生成于 $(date -u +%Y-%m-%dT%H:%M:%SZ))

# 服务器: 只听本地, 由反代 (Caddy/Nginx) 转发外部流量
SERVER_HOST=127.0.0.1
SERVER_PORT=$DEFAULT_PORT

# 数据库: SQLite (50 用户场景够用; 多机请切 postgres)
DATABASE_DRIVER=sqlite

# 缓存: 留空走 in-memory (单机够用; 多机切 Redis)
# REDIS_HOST=127.0.0.1
# REDIS_PORT=6379

# 管理员密码 (自动生成, 首次登录后请妥善保管, 也可改这里)
ADMIN_PASSWORD=$ADMIN_PWD

# 日志: debug / info / warn / error
LOG_LEVEL=info
EOF
    chown "$SERVICE_USER:$SERVICE_USER" "$INSTALL_DIR/.env"
    chmod 600 "$INSTALL_DIR/.env"
    echo
    echo "    ┌─────────────────────────────────────────────────┐"
    echo "    │  ADMIN_PASSWORD: $ADMIN_PWD"
    echo "    └─────────────────────────────────────────────────┘"
    echo "    (也可在 $INSTALL_DIR/.env 修改)"
    echo
else
    say ".env 已存在,保留不动"
fi

chown -R "$SERVICE_USER:$SERVICE_USER" "$INSTALL_DIR"

# ---- 7) systemd unit ----
say "安装 systemd unit"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
UNIT_SRC="$SCRIPT_DIR/../deploy/cc-bridge.service"
[ -f "$UNIT_SRC" ] || UNIT_SRC="$SCRIPT_DIR/cc-bridge.service"  # 同目录兜底
[ -f "$UNIT_SRC" ] || die "找不到 cc-bridge.service 单元文件 (期望在 deploy/ 或脚本同目录)"

install -m 0644 "$UNIT_SRC" "/etc/systemd/system/${SERVICE_NAME}.service"
systemctl daemon-reload
systemctl enable "$SERVICE_NAME" >/dev/null 2>&1 || true

# ---- 8) 启动 + 健康检查 ----
say "启动 $SERVICE_NAME"
systemctl restart "$SERVICE_NAME"

say "等待 /readyz 通过 (最多 30 秒)"
for i in $(seq 1 30); do
    if curl -fsS "http://127.0.0.1:$DEFAULT_PORT/readyz" >/dev/null 2>&1; then
        say "✓ 服务就绪 (用时 ${i}s, 版本 $TAG)"
        break
    fi
    sleep 1
    [ "$i" = "30" ] && {
        warn "30s 内 /readyz 没通过,检查 journalctl -u $SERVICE_NAME"
        systemctl status "$SERVICE_NAME" --no-pager || true
        exit 1
    }
done

# ---- 9) 完成 ----
echo
echo "======================================================"
echo "  cc-bridge 安装成功"
echo "------------------------------------------------------"
echo "  版本     : $TAG"
echo "  路径     : $INSTALL_DIR"
echo "  本地访问 : http://127.0.0.1:$DEFAULT_PORT"
echo "  状态查看 : systemctl status $SERVICE_NAME"
echo "  日志     : journalctl -u $SERVICE_NAME -f"
echo "  停止     : sudo systemctl stop $SERVICE_NAME"
echo "  升级     : sudo $INSTALL_DIR/scripts/upgrade.sh"
echo "======================================================"
echo
echo "下一步:"
echo "  1. 配反代 + TLS (deploy/Caddyfile.example 抄一份到 /etc/caddy/Caddyfile)"
echo "  2. 浏览器打开 http://<服务器IP>:$DEFAULT_PORT (临时直连验证)"
echo "  3. 用 ADMIN_PASSWORD 登录, 添加账号 + token"
