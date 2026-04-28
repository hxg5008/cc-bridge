#!/usr/bin/env bash
# cc-bridge 一行命令自动部署脚本
#
# 在干净的 Ubuntu/Debian 服务器上跑:
#
#   # 公开仓库 / 已 build 出 release:
#   curl -fsSL https://raw.githubusercontent.com/YOUR_GH_USER/cc-bridge/ccb/scripts/bootstrap.sh | sudo bash
#
#   # 私有仓库:
#   curl -fsSL https://raw.githubusercontent.com/YOUR_GH_USER/cc-bridge/ccb/scripts/bootstrap.sh \
#     | sudo GH_TOKEN=ghp_xxx bash
#
#   # 指定版本:
#   curl -fsSL .../bootstrap.sh | sudo CC_BRIDGE_TAG=v1.8.5 bash
#
# 流程:
#   1. 装 curl/ca-certificates/tar
#   2. 从 GitHub Release 拉最新 cc-bridge-deploy.tar.gz (含所有脚本 + systemd unit)
#   3. 解压到 /tmp/cc-bridge-deploy
#   4. 跑 install.sh

set -euo pipefail

REPO="${CC_BRIDGE_REPO:-YOUR_GH_USER/cc-bridge}"
TAG="${CC_BRIDGE_TAG:-latest}"
WORKDIR="${CC_BRIDGE_WORKDIR:-/tmp/cc-bridge-deploy}"

say()  { printf '\033[1;32m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[1;33m!!  %s\033[0m\n' "$*" >&2; }
die()  { printf '\033[1;31mxx  %s\033[0m\n' "$*" >&2; exit 1; }

[ "$(id -u)" = "0" ] || die "请用 sudo bash"

# ---- 装基础工具 ----
say "[1/4] 装基础工具 (curl, ca-certificates, tar)"
export DEBIAN_FRONTEND=noninteractive
if command -v apt-get >/dev/null 2>&1; then
    apt-get update -qq
    apt-get install -y -qq curl ca-certificates tar coreutils >/dev/null
elif command -v yum >/dev/null 2>&1; then
    yum install -y -q curl ca-certificates tar coreutils
else
    die "未识别的包管理器 (只支持 apt/yum)"
fi

# ---- 拉 deploy 包 ----
AUTH=()
[ -n "${GH_TOKEN:-}" ] && AUTH=(-H "Authorization: Bearer $GH_TOKEN")

if [ "$TAG" = "latest" ]; then
    say "[2/4] 查询最新 release"
    TAG=$(curl -fsSL "${AUTH[@]}" \
        "https://api.github.com/repos/$REPO/releases/latest" \
        | grep -oP '"tag_name":\s*"\K[^"]+' || true)
    [ -n "$TAG" ] || die "查询 latest tag 失败 (私有仓库需 GH_TOKEN=ghp_xxx)"
fi
say "    使用版本 $TAG"

DL_URL="https://github.com/$REPO/releases/download/$TAG/cc-bridge-deploy.tar.gz"
say "[3/4] 下载 $DL_URL"
rm -rf "$WORKDIR"
mkdir -p "$WORKDIR"
TMP_TGZ=$(mktemp)
trap 'rm -f "$TMP_TGZ"' EXIT
curl -fL "${AUTH[@]}" "$DL_URL" -o "$TMP_TGZ" \
    || die "下载部署包失败 (检查 release 是否包含 cc-bridge-deploy.tar.gz)"
tar xzf "$TMP_TGZ" -C "$WORKDIR" --strip-components=1
chmod +x "$WORKDIR/scripts/"*.sh

# ---- 跑 install.sh ----
say "[4/4] 启动 install.sh"
cd "$WORKDIR"

# 把环境变量传下去 (让 install.sh 能拿到 GH_TOKEN)
if [ -n "${GH_TOKEN:-}" ]; then
    export GH_TOKEN
fi
exec "$WORKDIR/scripts/install.sh" "$TAG"
