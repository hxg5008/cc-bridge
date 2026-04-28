# cc-bridge CI/CD 部署指南（GitHub Actions + ghcr.io）

**目标**：你的 2 核服务器**永不再编译**。代码改动 → 推 GitHub → CI 自动构建镜像 → 服务器只 `docker compose pull` 拉镜像（10 秒）→ 上线。

---

## 工作流总览

```
[本地 Windows]                  [GitHub]                         [Debian 12 服务器]
edit code  ────push─────►    ccb 分支
bump .version                       │
git push                            │
                                    ▼
                            release.yml 触发
                            (GitHub Actions, 免费 Linux runner)
                              - cargo build --release
                              - npm run build
                              - docker buildx multi-arch
                              - 推送到 ghcr.io
                                    │
                                    ▼
                            ghcr.io/hxg5008/cc-bridge:latest
                                    │
                                    ▼  docker compose pull
                                                                  ┌──────────────┐
                                                                  │ 服务器零编译 │
                                                                  │ 10 秒上新版  │
                                                                  └──────────────┘
```

每次更新只需 3 步：
1. 本地改代码 + 改 `.version` 版本号 + push
2. 等 GitHub Actions 跑完（~5-10 分钟，**用的是 GitHub 免费 runner，跟你两边都没关系**）
3. 服务器 `docker compose pull && docker compose up -d`（**10 秒**，**零 CPU 占用**）

---

## 一次性配置

### Step 1：在 GitHub 上创建私有仓库

1. 登 [github.com](https://github.com) → 右上 + → New repository
2. 仓库名建议：`cc-bridge`（与镜像名一致）
3. **Visibility 选 Private**（重要：你的反代核心代码不公开）
4. 不勾任何初始化（README/license/.gitignore），创建空仓库
5. 创建后地址应该是：`https://github.com/hxg5008/cc-bridge.git`

### Step 2：本地把代码 push 上去

`.version` 和 `docker-compose.yml` 里的 GitHub 用户名**已经填好为 `hxg5008`**，无需改动。

在 **本地 Windows** 解压 `cc-bridge-debian12.tar.gz` 后的 `cc-bridge/` 目录里：

```bash
cd cc-bridge

# 初始化 git 并 push
git init
git checkout -b ccb              # release.yml 监听 ccb 分支
git add .
git commit -m "init"
git remote add origin https://github.com/hxg5008/cc-bridge.git
git push -u origin ccb
```

> 第一次 push 系统会让你登录。如果用密码登录失败，去 GitHub Settings → Developer settings → Personal access tokens → 生成一个 classic token（勾 `repo` + `write:packages`），用它当密码登录。

### Step 3：触发第一次构建

```bash
# 改 .version 里的 version 号（比如 1.8.10 → 1.8.11）会触发 release.yml
nano .version       # 改 version=1.8.11
git add .version
git commit -m "release v1.8.11"
git push
```

去 GitHub 仓库 → Actions 标签，应该看到一个跑起来的 "Build & Release" workflow。等它绿勾完成（~5-10 分钟首次，后面有缓存会快）。

跑完后：
- ghcr.io 上有了 `ghcr.io/hxg5008/cc-bridge:latest` 和 `:v1.8.11`
- GitHub 仓库的 Releases 页有了 v1.8.11 release（含 deploy bundle）

### Step 4：服务器配置 PAT 让它能拉私有镜像

私有仓库的镜像需要登录才能 pull。在 GitHub 上生成一个**只读 PAT**：

1. GitHub → Settings → Developer settings → Personal access tokens → **Tokens (classic)** → Generate new token (classic)
2. Note 写 `cc-bridge-server-pull`，Expiration 选 **No expiration**
3. **只勾**：`read:packages`
4. 生成后**立刻复制 token**（页面关掉就再也看不到了），格式是 `ghp_xxxxxxxxxxxx`

到服务器上一次性登录：

```bash
# SSH 进 Debian 服务器
echo "ghp_xxxxxxxxxxxxxxxxxxxxxxxxxx" | docker login ghcr.io -u hxg5008 --password-stdin
# 应输出 "Login Succeeded"
```

> 这个 token 仅用于 pull，存在 `/root/.docker/config.json`。重装服务器才需要重做。

### Step 5：服务器首次部署

```bash
# 把项目代码先拉到服务器（用得上 docker-compose.yml 和 .env.example）
cd /opt
git clone https://github.com/hxg5008/cc-bridge.git    # 私有仓库需要 PAT 鉴权或 SSH key
cd cc-bridge
git checkout ccb

# 配 .env（参考之前的 cc-bridge-debian12-部署步骤.md 第 3 步生成密码）
cp .env.example .env
nano .env       # 改 ADMIN_PASSWORD / POSTGRES_PASSWORD / DATABASE_DSN

# 一次性拉镜像 + 启动（不会编译，因为 ghcr 已经有镜像）
docker compose pull
docker compose up -d
docker compose ps    # 两行都该是 Up + healthy
```

---

## 后续每次更新（**这才是你最关心的**）

### 本地 Windows：

```bash
cd /path/to/cc-bridge
# ... 编辑代码 ...

# 改 .version 文件，把 version 数字加一（比如 1.8.11 → 1.8.12）
nano .version

git add .
git commit -m "你的更新说明"
git push

# 去 GitHub Actions 看构建（~3-5 分钟有缓存命中）
```

### 服务器 Debian：

```bash
cd /opt/cc-bridge
git pull             # 同步代码（主要是 docker-compose.yml 等配置变化）
docker compose pull  # 拉新镜像，~10 秒
docker compose up -d # 重启容器，~5 秒
```

**完事**。CPU 占用：~5%（仅 docker daemon 重启容器）。**永不编译**。

---

## 常见问题

| 问题 | 原因 | 解决 |
|---|---|---|
| `docker compose pull` 报 `unauthorized: authentication required` | 服务器没登录 ghcr.io 或 PAT 过期 | 重做 Step 4：`docker login ghcr.io -u USER` |
| GitHub Actions 失败，红 X | 通常是代码编译错或测试 fail | 点进 Actions log 看具体错；本地 `cargo build --release` 先验证 |
| 改了代码但 push 后 Actions **没触发** | 你改的不是 `.version` 文件 | release.yml **只在 .version 变化时触发**。每次发版必须 bump 版本号 |
| Actions 跑完了但服务器 pull 不到新镜像 | 你 pull 的是 `:latest`，新镜像还没推完 | 等 1-2 分钟，或直接 `docker compose pull` 重试 |
| 想要"push 即构建" 不要每次 bump version | 现在 release.yml 设计是只发布版本号变化 | 可以加一个 `build-dev.yml`，监听 push 但只推 `:dev` tag。改服务器的 image 指向 `:dev` 即可。需要时跟我说 |

---

## 安全提醒

- **PAT 只给 `read:packages`**，不要给写权限
- **私有仓库不要 push `.env` 文件**（已经在 `.gitignore` 里）
- 推送代码前 `git status` 看一眼，确认没意外加上密码 / token
- 服务器的 PAT 存在 `/root/.docker/config.json` 用 base64 编码（不算加密），**别让别人 SSH 上你的服务器**

---

## 镜像地址速查

```
ghcr.io/hxg5008/cc-bridge:latest      # 永远指向最新版
ghcr.io/hxg5008/cc-bridge:v1.8.11     # 指定版本（用于回滚）
ghcr.io/hxg5008/cc-bridge:1.8.11      # 同上，无 v 前缀
```

回滚到老版本：

```bash
cd /opt/cc-bridge
docker compose down
# 临时改 docker-compose.yml 的 image: 行 → ghcr.io/hxg5008/cc-bridge:v1.8.10
nano docker-compose.yml
docker compose up -d
```

---

## TL;DR

设置一次（10 分钟）→ 之后每次更新：

```bash
# 本地：
echo "version=$(($(grep -oP '(?<=version=)[\d.]+$' .version | awk -F. '{$NF=$NF+1;print}' OFS=.))" > .version  # 复杂; 直接 nano 改最方便
git push

# 服务器：
docker compose pull && docker compose up -d
```

**服务器从此告别编译。**
