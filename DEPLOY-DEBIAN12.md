# cc-bridge 部署指南 — Debian 12 (Bookworm)

本指南把"一台干净的 Debian 12 服务器 → 跑起来 cc-bridge"这条路径走完，全程约 10 分钟。

---

## 0. 你需要什么

- 一台 Debian 12 服务器，2 vCPU / 2GB RAM 起步（4GB 推荐）
- 公网 IP 或反代域名（建议放 Caddy / Nginx 后面终止 TLS）
- root / sudo 权限

> **国内服务器**: 后续每个账号要单独配 `proxy_url` 才能连 anthropic.com / chatgpt.com 上游。

---

## 1. 装 Docker (一次性)

```bash
curl -fsSL https://get.docker.com | sh
sudo apt install -y docker-compose-plugin

# 让你当前用户能直接用 docker（不必每次 sudo），需要重新登录生效
sudo usermod -aG docker $USER
newgrp docker

# 验证
docker version
docker compose version
```

---

## 2. 解压代码

把 `cc-bridge-debian12.tar.gz` 上传到服务器，然后：

```bash
mkdir -p /opt && cd /opt
tar xzf ~/cc-bridge-debian12.tar.gz       # 路径按你上传的位置改
cd cc-bridge
```

---

## 3. 配置 `.env`

```bash
cp .env.example .env
nano .env
```

**至少要改这几个**：

```env
# 必改：管理后台密码（默认 admin，生产环境必须换）
ADMIN_PASSWORD=改成你的强密码

# 必改：Postgres 密码（DSN 里的密码也要同步改）
POSTGRES_PASSWORD=同样改成强密码
DATABASE_DSN=postgres://cc-bridge:同样改成强密码@postgres:5432/cc-bridge?sslmode=disable

# 公网部署务必改成只听本地，由前面的 Caddy/Nginx 转发
SERVER_HOST=127.0.0.1
```

> 推荐用 `openssl rand -base64 24` 生成密码。

---

## 4. 启动

```bash
docker compose up -d --build
```

首次会触发本地 build（前端 vite + Rust release），全程 5–10 分钟，看 CPU。后续重启秒级完成。

---

## 5. 验证

```bash
# 看容器状态（两个都该是 Up / healthy）
docker compose ps

# 看日志
docker compose logs -f cc-bridge

# 健康检查
curl http://127.0.0.1:5674/livez       # 应返回 ok
curl http://127.0.0.1:5674/readyz      # 应返回 ready
```

浏览器开 `http://<服务器 IP>:5674`（如果还没配反代）或 `https://你的域名`，密码是 `.env` 里的 `ADMIN_PASSWORD`。

---

## 6. 反代 + HTTPS（强烈建议）

最简方案 Caddy（自动 Let's Encrypt）：

```bash
sudo apt install -y debian-keyring debian-archive-keyring apt-transport-https
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | sudo gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' | sudo tee /etc/apt/sources.list.d/caddy-stable.list
sudo apt update && sudo apt install -y caddy

# 编辑 /etc/caddy/Caddyfile
sudo tee /etc/caddy/Caddyfile <<'EOF'
你的域名.com {
    reverse_proxy 127.0.0.1:5674
}
EOF

sudo systemctl reload caddy
```

DNS 把域名指到这台机器后，Caddy 自动签证书 + 续期。

---

## 7. 常用运维命令

```bash
# 重启
docker compose restart cc-bridge

# 升级（拉新代码后）
git pull
docker compose up -d --build

# 看错误日志
docker compose logs --tail=200 cc-bridge | grep -iE 'error|warn'

# 进 Postgres
docker compose exec postgres psql -U cc-bridge -d cc-bridge

# 备份 PG
docker compose exec -T postgres pg_dump -U cc-bridge cc-bridge | gzip > backup-$(date +%F).sql.gz

# 恢复 PG
gunzip < backup.sql.gz | docker compose exec -T postgres psql -U cc-bridge -d cc-bridge

# 完全停掉（保留数据）
docker compose down

# 完全停掉 + 删数据（不可逆）
docker compose down -v
```

---

## 8. 多副本（可选）

单机够 50 个账号 / 中等流量。要扩展到多机时启用 Redis（粘性会话跨机器需要它）：

```bash
# .env 加：
REDIS_HOST=redis
REDIS_PASSWORD=改成你的强密码

# 启动时带 redis profile：
docker compose --profile redis up -d --build
```

多台机器共享同一个 PostgreSQL + Redis，前面 Caddy/Nginx round-robin 即可。

---

## 9. 升级到新版本

把新代码替换 `/opt/cc-bridge` 后：

```bash
cd /opt/cc-bridge
docker compose up -d --build
```

数据库 schema 会在启动时自动 ALTER。**升级前建议先备份 PG**（见上面 `pg_dump`）。

---

## 故障排查速查

| 现象 | 常见原因 | 解决 |
|---|---|---|
| `docker compose up` 卡在 build 阶段 | 网络慢，npm/cargo registry 拉不动 | 给 docker 配国内镜像；或 `docker build` 加 `--build-arg HTTP_PROXY=...` |
| `cc-bridge` 反复重启，日志 `connection refused (postgres)` | postgres 还没 healthy 完 | 等 30 秒；持续报错查 `docker compose logs postgres` |
| `/readyz` 返回 503 | DB 连不上 | 核对 `.env` 里 DSN 用户名/密码和 `POSTGRES_*` 是否一致 |
| 前端能开但点账号"测试"全失败 | 上游需要代理 | 在每个账号设置里填 `proxy_url`，例如 `http://user:pass@host:port` |
| 上游返回 403 / "Just a moment" | Cloudflare 风控 | 换代理 IP，或检查 craftls 指纹是否被识破（看 README 里的 fingerprint 段） |
| `database is locked` | **不会出现** — v1.8.x 起已改 PG | 如果你看到这个，说明你在用老的 SQLite 版本，按本指南重新部署 |

---

## 文件位置速查

| 用途 | 路径 |
|---|---|
| 配置 | `/opt/cc-bridge/.env` |
| 数据（PG） | docker named volume `cc-bridge_postgres-data`（用 `docker volume inspect` 看物理路径） |
| 日志 | `docker compose logs cc-bridge` / `docker compose logs postgres` |
| 自动运行配置 | docker daemon 控制（`systemctl enable docker`） |

---

部署完毕。账号管理 / API token / SessionKey 导入这些操作直接在 Web UI 里做即可。
