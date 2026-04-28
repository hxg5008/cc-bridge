#!/usr/bin/env bash
# cc-bridge 一键升级脚本
#
# 自动:
#   1. 查询 GitHub Releases 最新 tag
#   2. 跟当前 CURRENT_VERSION 对比, 已是最新则退出
#   3. 备份当前 binary + DB
#   4. 下载新 binary, 替换, systemctl restart
#   5. 等 /readyz 通过 → 成功 (清理 7 天前的备份)
#   6. /readyz 30s 没通过 → 自动回滚到旧 binary
#
# 用法:
#   sudo ./upgrade.sh                    # 升级到 latest
#   sudo ./upgrade.sh v1.8.5             # 升级到指定版本 (可降级回滚老版本)
#   sudo ./upgrade.sh --check            # 只检查是否有新版本, 不升级
#   sudo ./upgrade.sh --force            # 强制重装当前 latest

set -euo pipefail

REPO="${CC_BRIDGE_REPO:-hxg5008/cc-bridge}"
INSTALL_DIR="${CC_BRIDGE_DIR:-/opt/cc-bridge}"
SERVICE_USER="${CC_BRIDGE_USER:-ccbridge}"
SERVICE_NAME="cc-bridge"

say()  { printf '\033[1;32m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[1;33m!!  %s\033[0m\n' "$*" >&2; }
die()  { printf '\033[1;31mxx  %s\033[0m\n' "$*" >&2; exit 1; }

[ "$(id -u)" = "0" ] || die "请用 root 或 sudo 跑"
[ -d "$INSTALL_DIR" ] || die "$INSTALL_DIR 不存在,先跑 install.sh"
[ -f "$INSTALL_DIR/.env" ] || die "$INSTALL_DIR/.env 不存在,先跑 install.sh"

source "$INSTALL_DIR/.env"
PORT="${SERVER_PORT:-5674}"

ARCH=$(uname -m)
case "$ARCH" in
    x86_64|amd64)  ARTIFACT="cc-bridge-linux-amd64" ;;
    aarch64|arm64) ARTIFACT="cc-bridge-linux-arm64" ;;
    *)             die "不支持的架构: $ARCH" ;;
esac

# 解析参数
TAG="latest"
FORCE=0
CHECK_ONLY=0
for arg in "$@"; do
    case "$arg" in
        --force)   FORCE=1 ;;
        --check)   CHECK_ONLY=1 ;;
        --help|-h) echo "Usage: $0 [TAG|latest|--check|--force]"; exit 0 ;;
        v*)        TAG="$arg" ;;
        *)         die "未知参数: $arg" ;;
    esac
done

# ---- 1) 查询版本 ----
AUTH=()
[ -n "${GH_TOKEN:-}" ] && AUTH=(-H "Authorization: Bearer $GH_TOKEN")

if [ "$TAG" = "latest" ]; then
    say "查询最新 release"
    TAG=$(curl -fsSL "${AUTH[@]}" \
        "https://api.github.com/repos/$REPO/releases/latest" \
        | grep -oP '"tag_name":\s*"\K[^"]+' || true)
    [ -n "$TAG" ] || die "查询失败 (私有仓库需 GH_TOKEN=ghp_xxx)"
fi

CURRENT="(none)"
[ -f "$INSTALL_DIR/CURRENT_VERSION" ] && CURRENT=$(cat "$INSTALL_DIR/CURRENT_VERSION")

echo "    当前: $CURRENT"
echo "    目标: $TAG"

if [ "$CHECK_ONLY" = "1" ]; then
    [ "$CURRENT" = "$TAG" ] && echo "    → 已是最新" || echo "    → 有新版本"
    exit 0
fi

if [ "$FORCE" != "1" ] && [ "$CURRENT" = "$TAG" ]; then
    say "已是最新版本,跳过 (加 --force 强制重装)"
    exit 0
fi

# ---- 2) 备份 ----
TS=$(date +%Y%m%d-%H%M%S)
BAK_DIR="$INSTALL_DIR/backups/$TS"
mkdir -p "$BAK_DIR"
say "备份到 $BAK_DIR"
[ -f "$INSTALL_DIR/cc-bridge" ] && cp "$INSTALL_DIR/cc-bridge" "$BAK_DIR/" || true
[ -f "$INSTALL_DIR/data/claude-code-gateway.db" ] \
    && cp "$INSTALL_DIR/data/claude-code-gateway.db" "$BAK_DIR/" || true
[ -f "$INSTALL_DIR/CURRENT_VERSION" ] && cp "$INSTALL_DIR/CURRENT_VERSION" "$BAK_DIR/" || true

# ---- 3) 下载 ----
DL_URL="https://github.com/$REPO/releases/download/$TAG/${ARTIFACT}.tar.gz"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

say "下载 $DL_URL"
curl -fL "${AUTH[@]}" "$DL_URL" -o "$TMP/cc-bridge.tar.gz" \
    || die "下载失败 (检查 tag 是否存在 / 私有仓库 token)"

if curl -fsL "${AUTH[@]}" "${DL_URL}.sha256" -o "$TMP/cc-bridge.tar.gz.sha256" 2>/dev/null; then
    say "校验 SHA256"
    (cd "$TMP" && sha256sum -c cc-bridge.tar.gz.sha256) \
        || die "SHA256 校验失败,可能下载损坏"
fi

tar xzf "$TMP/cc-bridge.tar.gz" -C "$TMP"
[ -f "$TMP/$ARTIFACT" ] || die "tarball 内未找到 $ARTIFACT"

# ---- 4) 替换 binary + 重启 ----
say "替换 binary 并重启服务"
install -m 0755 -o "$SERVICE_USER" -g "$SERVICE_USER" "$TMP/$ARTIFACT" "$INSTALL_DIR/cc-bridge"
echo "$TAG" > "$INSTALL_DIR/CURRENT_VERSION"
chown "$SERVICE_USER:$SERVICE_USER" "$INSTALL_DIR/CURRENT_VERSION"

systemctl restart "$SERVICE_NAME"

# ---- 5) 健康检查 ----
say "等待 /readyz 通过"
for i in $(seq 1 30); do
    if curl -fsS "http://127.0.0.1:$PORT/readyz" >/dev/null 2>&1; then
        say "✓ 升级成功 (用时 ${i}s, 版本 $CURRENT → $TAG)"

        # 清理 7 天前的备份
        find "$INSTALL_DIR/backups" -maxdepth 1 -type d -mtime +7 \
            -exec rm -rf {} + 2>/dev/null || true

        exit 0
    fi
    sleep 1
done

# ---- 6) 自动回滚 ----
warn "30s 内 /readyz 没通过,自动回滚"
if [ -f "$BAK_DIR/cc-bridge" ]; then
    install -m 0755 -o "$SERVICE_USER" -g "$SERVICE_USER" \
        "$BAK_DIR/cc-bridge" "$INSTALL_DIR/cc-bridge"
    [ -f "$BAK_DIR/CURRENT_VERSION" ] \
        && cp "$BAK_DIR/CURRENT_VERSION" "$INSTALL_DIR/CURRENT_VERSION"
    systemctl restart "$SERVICE_NAME"
    warn "已回滚到 $CURRENT"
    journalctl -u "$SERVICE_NAME" --no-pager -n 30 || true
    exit 1
else
    die "没有备份可回滚 (首次安装失败), 检查 journalctl -u $SERVICE_NAME"
fi
