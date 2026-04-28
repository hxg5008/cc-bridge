# cc-bridge 部署指南

一键脚本 + systemd + 反代 TLS,单台 Ubuntu/Debian 服务器十分钟可上线。

---

## 系统要求

| 项 | 最低 | 推荐 |
|---|---|---|
| OS | Ubuntu 20.04+ / Debian 11+ | Ubuntu 22.04 LTS |
| 架构 | x86_64 / aarch64 | x86_64 |
| CPU | 1 核 | 2 核 |
| 内存 | 256 MB | 512 MB |
| 磁盘 | 200 MB | 5 GB |
| 网络 | 公网 IP + 域名 (反代 TLS 用) | 海外节点更稳 (chatgpt.com / api.anthropic.com 直连) |

---

## 首次安装 (10 分钟)

### 模式 A:**一行命令全自动**(最推荐)

```bash
# 公开仓库:
curl -fsSL https://raw.githubusercontent.com/YOUR_GH_USER/cc-bridge/ccb/scripts/bootstrap.sh | sudo bash

# 私有仓库:
curl -fsSL https://raw.githubusercontent.com/YOUR_GH_USER/cc-bridge/ccb/scripts/bootstrap.sh \
    | sudo GH_TOKEN=ghp_xxxxxxxxxx bash
```

bootstrap.sh 会自动:装依赖 → 拉 deploy 包 → 跑 install.sh → 验证 readiness。整个过程约 1 分钟,结束时打印随机生成的 ADMIN_PASSWORD。

### 模式 B:本地下载 deploy 包后手动跑(适合受限网络环境)

```bash
# 1. 拉部署包
curl -fL https://github.com/YOUR_GH_USER/cc-bridge/releases/latest/download/cc-bridge-deploy.tar.gz \
    | tar xz
cd cc-bridge-deploy

# 2. (私有仓库) 准备 GitHub PAT 用于下载 binary
#    https://github.com/settings/tokens → Fine-grained PAT, 勾 Contents: Read
export GH_TOKEN=ghp_xxxxxxxxxxxxxxxxxxxx

# 3. 一键安装最新 release
sudo -E ./scripts/install.sh

# 等 30 秒,会输出 ADMIN_PASSWORD,记下来
```

### 模式 C:Ansible 批量部署多台机器

```bash
# 1. 准备 inventory: hosts.ini
echo '[cc-bridge]' > hosts.ini
echo 'server1 ansible_host=1.2.3.4 ansible_user=root' >> hosts.ini
echo 'server2 ansible_host=5.6.7.8 ansible_user=root' >> hosts.ini

# 2. 一行批量部署
GH_TOKEN=ghp_xxx ansible-playbook -i hosts.ini deploy/ansible-playbook.yml

# 后续升级
ansible-playbook -i hosts.ini deploy/ansible-playbook.yml --tags upgrade
```

### 模式 D:cloud-init 机器创建时自动部署

把 `deploy/cloud-init.yaml` 内容粘到云厂商的 "User Data" 字段(改 GH_TOKEN_VALUE_HERE 和域名),机器开机即装好 cc-bridge + Caddy + 防火墙。

安装脚本会做:
- 创建 `ccbridge` 系统用户
- 装 binary 到 `/opt/cc-bridge/cc-bridge`
- 写默认 `.env` (含随机生成的强密码)
- 注册 systemd unit
- 启动并等 `/readyz` 通过

---

## 常用脚本

所有脚本在 `scripts/` 目录,需要 `sudo`:

| 命令 | 作用 |
|---|---|
| `sudo ./scripts/install.sh [tag]` | 首装,可指定版本 |
| `sudo ./scripts/upgrade.sh` | 升级到最新 release,失败自动回滚 |
| `sudo ./scripts/upgrade.sh v1.8.5` | 切到指定版本 (可向下回滚) |
| `sudo ./scripts/upgrade.sh --check` | 只检查是否有新版,不动 |
| `sudo ./scripts/start.sh` | 启动 (含 readiness 等待) |
| `sudo ./scripts/stop.sh` | 优雅停止 (SIGTERM 等 30s 排空) |
| `sudo ./scripts/restart.sh` | 重启 |
| `./scripts/status.sh` | 看版本 / health / 关键 metrics / 最近日志 |
| `sudo ./scripts/uninstall.sh` | 卸载 (保留数据) |
| `sudo ./scripts/uninstall.sh --purge` | 彻底删除 (含 DB) |

直接看 systemd:
```bash
systemctl status cc-bridge
journalctl -u cc-bridge -f       # 实时日志
journalctl -u cc-bridge -n 100   # 最近 100 行
```

---

## 配反代 + TLS

### Caddy (推荐, 自动 Let's Encrypt)

```bash
# 装 Caddy
apt install -y debian-keyring debian-archive-keyring apt-transport-https
curl -1sLf https://dl.cloudsmith.io/public/caddy/stable/gpg.key \
    | gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
curl -1sLf https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt \
    | tee /etc/apt/sources.list.d/caddy-stable.list
apt update && apt install -y caddy

# 用模板
cp deploy/Caddyfile.example /etc/caddy/Caddyfile
vim /etc/caddy/Caddyfile         # 改 api.example.com 为你的域名
systemctl reload caddy
```

### Nginx (传统选项)

```bash
apt install -y nginx certbot python3-certbot-nginx
cp deploy/nginx.conf.example /etc/nginx/sites-available/cc-bridge
ln -s /etc/nginx/sites-available/cc-bridge /etc/nginx/sites-enabled/
vim /etc/nginx/sites-available/cc-bridge   # 改 api.example.com
nginx -t && systemctl reload nginx
certbot --nginx -d api.example.com
```

---

## 配置项 (.env)

| 变量 | 默认 | 说明 |
|---|---|---|
| `SERVER_HOST` | 127.0.0.1 | 监听地址 (公网部署务必只听本地, 反代转发) |
| `SERVER_PORT` | 5674 | 监听端口 |
| `DATABASE_DRIVER` | sqlite | sqlite \| postgres |
| `DATABASE_DSN` | 自动 | postgres 时填完整 DSN |
| `REDIS_HOST` | 留空 | 留空走 in-memory cache;多机部署填 redis 地址 |
| `ADMIN_PASSWORD` | 随机 | 管理后台密码 |
| `LOG_LEVEL` | info | debug / info / warn / error |

改 .env 后:`sudo ./scripts/restart.sh`

---

## 健康检查 / 监控

| 端点 | 用途 |
|---|---|
| `GET /livez` | 进程存活 (永远 200,只要进程在) |
| `GET /readyz` | DB 连通性,K8s readiness 用 |
| `GET /metrics` | Prometheus 文本格式指标 |

**这三个端点不能暴露给公网** (反代配置已经默认 deny)。

如果接 Prometheus,在内网开个 8080 端口或 VPN 暴露 `/metrics`:
- `ccbridge_gateway_requests_total{platform=...}` — 请求总数
- `ccbridge_gateway_errors_total{platform,status}` — 429/5xx 错误
- `ccbridge_oauth_refresh_total{platform,outcome}` — token refresh 成功/失败
- `ccbridge_openai_failover_total` — OpenAI 5xx 触发账号切换次数
- `ccbridge_openai_account_filtered_by_limit_total` — 账号被用量门禁过滤次数
- `ccbridge_accounts{platform,status}` — 当前账号状态分布
- `ccbridge_uptime_seconds` — 启动时长

告警建议 (Grafana / Alertmanager):
- 1 分钟内 oauth refresh failure > 5 次 → token 过期或账号坏
- 1 分钟内 5xx > 10 次 → 上游异常 (chatgpt.com / api.anthropic.com)
- failover_total 1 分钟内 > 5 → 账号池不够用

---

## 备份

SQLite 文件 `/opt/cc-bridge/data/claude-code-gateway.db`,加 cron:

```bash
# /etc/cron.d/cc-bridge-backup
0 * * * * ccbridge cp /opt/cc-bridge/data/claude-code-gateway.db \
    /opt/cc-bridge/backups/db-hourly-$(date +\%H).db 2>/dev/null
0 3 * * * ccbridge cp /opt/cc-bridge/data/claude-code-gateway.db \
    /opt/cc-bridge/backups/db-daily-$(date +\%a).db 2>/dev/null
```

或 Postgres → 标准 `pg_dump | gzip > backup.sql.gz`。

---

## 常见问题

**Q: 升级失败回滚后要怎么排查?**
A: `journalctl -u cc-bridge --since "5 min ago"`,常见原因:DB schema 不兼容(迁移失败) / 端口被占 / .env 配置错。修复后 `sudo ./scripts/upgrade.sh --force` 重试。

**Q: 怎么切到 Postgres?**
A: 先在 .env 改 `DATABASE_DRIVER=postgres` 和 `DATABASE_DSN=postgres://...`,然后**先用 `pg_dump` 把 SQLite 数据导过去** (网关本身不带数据迁移工具),最后 restart。

**Q: 多台机器分担流量怎么办?**
A: 走 Postgres + Redis (sticky session 跨机器需要 Redis),Caddy/Nginx 前置做 round-robin。每台机器各自跑 install.sh,共用同一个 DATABASE_DSN 和 REDIS_HOST。

**Q: 怎么验证防风控正常?**
A: `tcpdump -i any -A -s0 'host chatgpt.com'` 抓上游请求,看:
  - `User-Agent: codex_cli_rs/0.104.0`
  - `Originator: codex_cli_rs`
  - `session_id` 是 UUID 格式
  - body 里 `store: false`、`stream: true`、`instructions` 已填充

**Q: 怎么换镜像源 (国内)?**
A: 把 `.env` 的 cargo / npm 走代理,或者直接在海外构建 binary 后用 upgrade.sh 拉取。
