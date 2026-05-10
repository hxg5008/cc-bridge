# cc-bridge 改动 & 规划记录

最近一次更新：2026-05-10

记录已完成改动 + 待办项。已完成清单按时间顺序追加；规划项按优先级分级，每条解释 **要做什么 / 解决什么 / 为什么暂缓 / 触发条件**。

---

## ✅ 已完成改动清单（截至 2026-05-10）

总计 **41 项** 改动落盘。Schema migration 已升到 v4。168 个单元测试全过。

### Phase 1 — 上线前必修（14 项）

| 编号 | 项目 | 文件 | 解决问题 |
|---|---|---|---|
| N7 | PG advisory lock 串行化 migration | `src/store/db.rs` | 多实例同时启动并发跑 ALTER |
| N9 | reqwest::Client 按 (proxy, timeout_ms) 缓存 + pool_max_idle 64 + connect_timeout 10s + tcp_keepalive 30s | `src/tlsfp/tlsfp.rs` | 上游 TLS 握手减 90%+，p99 不再爆 |
| N6 | Redis acquire_slot 改 Lua 原子 + release 失败 metrics | `src/store/redis.rs` | 闪断不会让槽位永久占用 |
| N8 | MemoryStore 后台 GC + 硬上限 LRU 淘汰 | `src/store/memory.rs` | in-memory 模式不再 OOM |
| N5 | absorb flush per-account 互斥 + 全局 Semaphore(8) | `src/service/limit.rs` | burst 1000 个 429 不会打爆 DB pool |
| N1 | 优雅关停: flush_all + 45s 硬 deadline | `src/main.rs` + `src/service/limit.rs` | SIGTERM 时限流状态完整落盘 |
| N2 | OAuth refresh 错误归因: 4xx=永久写 auth_error / 5xx=临时不写 | `src/service/oauth.rs` + `src/service/account.rs` | 上游抖动不再误标账号失效 |
| - | header 黑名单 (debug 日志脱敏) | `src/service/gateway.rs` | authorization/cookie 不会被打到 debug 日志 |
| - | /admin/metrics 鉴权 + admin IP 失败 5 次锁 5 分钟 | `src/middleware/auth.rs` | 公开指标关上 + 防暴力穷举密码 |
| N14 | 删 "https://" 假日志 + 警示必须前置 HTTPS 反代 | `src/main.rs` | 运维不会被骗认为有 TLS |
| N4 | upstream read_timeout 300→180s + SlotHolder max_hold 240s | `src/tlsfp/tlsfp.rs` + `src/service/gateway.rs` | 上游卡死不再锁住账号槽 5 分钟 |
| N3 | session_hash 加 api_token_id 命名空间 `k{id}:sid` | `src/service/account.rs` | 防多租户 sticky 串号 |
| N10 | UsageSniffer scratch 64K → 32K | `src/service/gateway.rs` | 1000 SSE 节省一半临时缓冲 |
| - | Token DB LRU 30s 缓存 + admin 改 token 主动 invalidate | `src/store/token_store.rs` | 高 RPS 下 99%+ token 校验不打 DB |

### Phase 2 — 上线后 1-2 周（4 黄）

| 编号 | 项目 | 文件 |
|---|---|---|
| N16 | 删账号清理 LimitStore + per-account metrics | `src/service/account.rs` `delete_account` |
| N12 | `is_sonnet_rejection` 改 sonnet 系列前缀匹配 (sonnet_v2/4_5 都旁路, opus 保持全局拉黑) | `src/service/limit.rs` |
| N20 | 429 retry-after 兜底: reset_at=Some(过期) 也补 ban | `src/service/limit.rs` |
| - | readyz 同时 ping DB + Redis | `src/handler/router.rs` + `src/store/cache.rs` |

### 7 项推荐（性能 + 可观测性）

| # | 项目 | 文件 |
|---|---|---|
| 1 | DB pool 50→100 + max_lifetime 30min + idle_timeout 10min | `src/store/db.rs` |
| 2 | CORS 全局宽松策略 | `src/handler/router.rs` |
| 3 | 后台 flush_to_db 失败计数 `ccbridge_bg_flush_db_failure_total` | `src/service/metrics.rs` + `limit.rs` |
| 4 | OAuth fallback per-account 计数 ≥3 警告 | `src/service/account.rs` + `metrics.rs` |
| 5 | 启动后 60s sticky 命中已限流号计数 | `src/service/account.rs` |
| 6 | docker-compose 起 PG 改 dev-only (`CCBRIDGE_DEV=1`) | `src/store/db.rs` |
| 7 | tokio worker 注释更正 (available_parallelism 自带 cgroup-aware) | `src/main.rs` |

### 安全 + 阈值（3 项）

| # | 项目 | 文件 |
|---|---|---|
| 1 | 弱口令拒启动 (长度<8 拒 + 黑名单拒 + 非 ASCII 拒 + 长度<16 警告) | `src/main.rs` `enforce_strong_admin_password` |
| 2 | HIT_THRESHOLD 1.0 → 0.97 + WARN_THRESHOLD 0.90 软降权 | `src/service/limit.rs` |
| 3 | `select_by_priority` 改用 effective_priority 闭包，Claude 路径 `priority + limit_store.priority_penalty(id)` | `src/service/account.rs` |

### 第二轮差距分析后修的 5 项 (2026-05-10 下午)

| 编号 | 项目 | 文件 | 解决问题 |
|---|---|---|---|
| **A1** | PG advisory lock 改用 `pool.acquire()` 拿专属连接 (修真 bug) | `src/store/db.rs` | 之前 `pool.execute()` 让 lock 跟 session 立刻失效, 等于没生效 |
| **C2** | 全局 in-flight cap 1000 (env `CCBRIDGE_GLOBAL_INFLIGHT_CAP`) + axum body limit 2MB | `src/handler/router.rs` `global_concurrency_middleware` | 突发 5000 RPS 不再把 runtime 打爆, 超 cap 立刻 503 + Retry-After |
| **E1** | `record_account_disabled(reason_label)` + `classify_disable_reason()` 7 类标签 | `src/service/account.rs` + `metrics.rs` | Prom 配 `increase(...{reason="auth_403"}[5m]) > 5` 提前发现批量失效 |
| **C1** | per-token concurrency slot (env `CCBRIDGE_TOKEN_CONCURRENCY=20`) + `SlotHolder::chain_with()` 链式持有 | `src/service/gateway.rs` + `src/service/account.rs` `cache()` | 单用户 50 并发脚本不再吃光所有账号槽 |
| **C6** | per-account circuit breaker: 5 次连续 5xx → open 30s, 一次成功自动 close | `src/service/limit.rs` `CircuitBreakerState` + `gateway.rs` | Anthropic 区域故障时自动暂停该账号, 不再死循环 |

---

## 🟡 中期项（差距分析挖出，建议 1-2 周内做）

### A2. `try_flush_to_db` 加 timeout

**要做什么**：每个 flush 包 `tokio::time::timeout(10s)`，超时记 metric 后释放 permit；spawn 之前 `try_acquire_owned()` 拿不到立刻丢弃。

**解决什么**：DB 偶发 IO 抖动时，8 个 permit 全卡住会让所有账号 absorb 后的 flush spawn 排队，进程内存涨。

**位置**：`src/service/limit.rs` `try_flush_to_db`

**工时**：1 人天

---

### A3. SlotHolder Drop 失败有 metrics

**要做什么**：`tokio::runtime::Handle::try_current()` 拿不到时（runtime 关停期间）记 `ccbridge_slot_release_dropped_total` counter。

**解决什么**：runtime 关停时部分槽位释放静默失败 → Redis 上 INCR 留下幻影占用。

**位置**：`src/service/gateway.rs` `Drop for SlotHolder`

**工时**：0.5 人天

---

### A4. ADMIN_FAIL_TRACKER 后台 GC

**要做什么**：和 MemoryStore 同款，启动 spawn 60s 周期 task 清理 `lock_until < now-1h && first_fail_at < now-1h` 的条目，硬上限 50K。

**解决什么**：攻击者用伪造的 X-Forwarded-For 头打 admin 不需要打穿密码就能撑爆 DashMap 内存。

**位置**：`src/middleware/auth.rs`

**工时**：0.5 人天

---

### B1. token LRU 缓存绕过 admin API 失效

**要做什么**：暴露 `/admin/tokens/refresh-cache` 端点；或加 60s 周期后台 task 全清。

**解决什么**：DBA 直接 SQL `UPDATE api_tokens SET status='inactive'` 时被禁的 token 还能继续用 30s。

**位置**：`src/store/token_store.rs`

**工时**：0.5 人天

---

### C3. 请求级 trace_id 全链路贯穿

**要做什么**：用 `tracing::instrument(fields(rid = %rid))` 包 `handle_request_inner`，全链路 span 自动带；同时把客户端 `x-request-id` 头透传出去（响应头 + 日志都加）。

**解决什么**：线上某用户报"我刚才发的请求超时了"，从日志根本拼不回那次具体调用。

**位置**：`src/service/gateway.rs`

**工时**：1 人天

---

### C4. list_schedulable 缓存击穿/惊群

**要做什么**：用 `tokio::sync::OnceCell` 或 singleflight 模式（一个 Mutex<Option<JoinHandle>>，第一个 miss 启动查询，后来的都 await 同一个 handle）。

**解决什么**：5s/10s TTL 过期那一瞬间所有进来的请求都 miss → 全部并发跑 `SELECT * FROM accounts`，DB pool 被相同 SELECT 占用。

**位置**：`src/store/account_store.rs`

**工时**：0.5 人天

---

### C5. metrics_handler 缓存账号列表

**要做什么**：复用 `list_schedulable` cache，或单独给 metrics 一个 30s memo。

**解决什么**：Prom 默认 15s scrape 一次 → 强制 DB 4 RPM 全表扫，账号上千时单次扫描几十 ms。

**位置**：`src/handler/router.rs` `metrics_handler`

**工时**：0.5 人天

---

### D1. SlotHolder max_hold 240s 加虚拟时钟测试

**要做什么**：用 `tokio::time::pause/advance` 构造一个永不结束的 stream，advance 250s，断言 slot 被释放。

**解决什么**：当前测试只覆盖到 drop / disarm，max_hold 路径无回归保护。

**位置**：`src/service/gateway.rs::tests`

**工时**：1 人天

---

## 🟠 谨慎项（行为有变化，要观察 / 要压测后才能定）

### 1. 上游 5xx 抖动重试 1 次

**要做什么**：gateway 在收到上游 502/503/504 但**还未拿到 SSE message_start 事件**时，自动换一个备选账号重试 1 次。已经吐过 message_start 之后的错误不重试（避免双扣 token）。

**解决什么**：Anthropic 边缘节点抖动率约 0.1-1%，目前所有 5xx 直接透传给客户端 → SSE 突然断流。改后 gateway 内部消化大部分 5xx。

**为什么暂缓**：
- "幂等可重试 vs 已开始消费 token 不能重试" 的边界判断有 corner case
- 测试需要 mock 上游构造各种半开 SSE 流场景
- 写错会导致同一请求被多个账号扣配额
- **C6 circuit breaker 已经能挡住"上游全挂"场景**，5xx 重试只优化"偶发抖动"

**触发条件**：客户端反馈"经常 SSE 断流"投诉率上来；或 metrics `ccbridge_gateway_errors_total{platform="claude",status="5xx"}` 持续 > 0.5%。

**实施估算**：1-2 人天（含集成测试）

---

## 🟢 长期项（按业务规模触发再做）

### 1. LimitStore Redis 化

**要做什么**：把 LimitStore 内存 DashMap 状态迁到 Redis hash + Lua 脚本写入。

**解决什么**：单实例顶不住要起 2+ 实例时，多实例必须共享限流状态，否则 A 实例已经 absorb 到 100% 但 B 实例还认为可用，会重复撞 429。

**触发条件**：单实例 CPU > 70% 持续 5min；或日活 > 500 用户。

**实施估算**：5 人天

---

### 2. 集成测试套件 (wiremock + k6)

**要做什么**：mock 上游 Anthropic / OpenAI 全套响应（200/429/5xx/SSE 中断/连接 reset），k6 做并发压测。

**解决什么**：每次改代码自动验证不破回归；上线前用真实压力数字定容量。

**触发条件**：团队 ≥ 2 人时；或上线后第一次出"修一个 bug 引入另一个 bug"时。

**实施估算**：5 人天

---

### 3. 真 TLS（axum-server + RustlsConfig）

**要做什么**：cc-bridge 直接 listen :443 + 加载证书。

**为什么暂缓**：前置 nginx/caddy 反代是行业标准，自动续证 + WAF + DDoS 缓解都得自己做。**这一项基本永远不需要**。

**触发条件**：除非完全脱离反代部署。

---

### 4. Token AES-GCM 加密落盘

**要做什么**：从 env 读 32 字节 KEK，所有 token 字段写入前 AES-GCM 包。

**解决什么**：DB 备份外泄 / DBA 凭据失守时，攻击者拿到 dump 也解不出 token。

**为什么暂缓**：
- KEK 管理是另一个工程问题（KMS / Vault），自己存 env 等于把鸡蛋换个篮子
- 一次性 migration 把存量 token 加密风险高（KEK 丢 = 所有号死透）
- 当前 PG 跑在 127.0.0.1，trusted boundary 内威胁模型暂时不成立

**触发条件**：迁到 RDS / 多团队共享 DB / 出现合规审计要求时。

**实施估算**：3-5 人天（含密钥管理 + 一次性数据迁移）

---

### 5. absorb_headers 立刻 spawn flush（替代 batch 阈值）

**要做什么**：把 absorb_headers 改成"任何状态变化都同步写一条很轻量的 (account_id, until_ts) 行到 ringbuffer 表"（write-through 而非 batch flush）。

**解决什么**：高峰期 OOM killer 选中 cc-bridge 时，刚刚撞 429 的 20 个账号状态全丢，重启后立即又把这 20 个号选出去打 429 → 又限流。

**为什么暂缓**：DB 写量增加，需要先压测验证 PG 吃得消；当前阈值 batch flush 已经能覆盖 95% 的常规场景。

**触发条件**：观察到多次"OOM 后限流状态丢失"事故。

**实施估算**：3 人天

---

### 6. 错误信息规范化

**要做什么**：错误体规范化为 `{"type":"capacity_exhausted","message":"...","retry_after":...}`，区分 5xx（系统问题，可重试）vs 429（配额问题，等待时间）。

**解决什么**：当前 `ServiceUnavailable("no available account: ...")` 直接暴露给客户端，泄漏内部状态；用户也搞不清是"额度耗尽"还是"系统故障"。

**位置**：`src/error.rs` + 全部 handler

**工时**：1.5 人天

---

### 7. PG / Redis 备份恢复 SOP

**要做什么**：写 `scripts/backup.sh` 用 `pg_dump --format=custom` + 上 S3，docs 里说明每天 cron 调用；Redis 配 RDB+AOF 持久化文档。

**解决什么**：PG 数据卷损坏时所有 token / OAuth 凭证全丢，需要让客户重新登录所有账号。

**工时**：0.5 人天

---

## 📌 当前状态评估（2026-05-10）

> **可以承接 200-300 daily-active 用户的商业流量。**

主要差距已经堵上：
- ✅ migration 真正串行（A1 已修，之前是 bug）
- ✅ 全局 OOM 防护（C2 in-flight cap + body limit）
- ✅ 单用户脚本拖垮全平台 → 防住（C1 per-token concurrency）
- ✅ Anthropic 区域故障时不死循环（C6 circuit breaker）
- ✅ 账号批量失效告警链就位（E1 metrics）

剩余 8 项 🟡 中期项工时约 5 人天，可在上线后 1-2 周内陆续做。

500+ 用户场景需要做 🟢 LimitStore Redis 化（5 人天）；其它 🟢 项按触发条件做。
