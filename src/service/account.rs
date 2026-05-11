use chrono::Utc;
use rand::Rng;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{info, warn};
use uuid::Uuid;
// parking_lot::Mutex 替 std::sync::Mutex (修 N1):
// std Mutex 持锁时 panic 会让锁中毒, 所有后续 lock() 全 panic, 整个 cooldown
// 子系统永久挂掉; parking_lot 不会中毒, panic 后下次 lock() 仍可用。
use parking_lot::Mutex;

use crate::error::AppError;
use crate::model::account::{Account, AccountAuthType};
use crate::service::limit::LimitStore;
use crate::service::rewriter::ClientType;
use crate::store::account_store::AccountStore;
use crate::store::cache::CacheStore;

const STICKY_SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const OAUTH_REFRESH_BUFFER_SECONDS: i64 = 5 * 60;
const OAUTH_LOCK_TTL: Duration = Duration::from_secs(30);

/// 账号在 dashboard / 卡片上显示的分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountCategory {
    /// 当前完全可调度 (status=active + 无 auth_error + LimitStore 全过)。
    Available,
    /// 暂时撞上游限流, 会自动恢复 (5h/7d 窗口满 / 短期 ban / RPM-TPM 预抢 / 全局 Rejected)。
    RateLimited,
    /// Token 失效 (status=active 但有 auth_error, 或 status=error)。需要人工重新授权。
    Invalid,
    /// 上游官方封禁 (status=disabled + reason 含 403/认证失败/violation)。
    Banned,
    /// 管理员手动停用 (status=disabled 但非"封禁"判定)。
    Stopped,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AccountCategorization {
    pub category: AccountCategory,
    /// 给 UI 显示的子原因文字 (例如 "5 小时窗口已用 100.0%" / "手动停用" / token 错误信息)。
    pub reason: String,
    /// 该状态预计恢复时刻 (限流中才有, 用于 UI 倒计时)。
    pub recovers_at: Option<chrono::DateTime<chrono::Utc>>,
}
const OAUTH_WAIT_RETRY: Duration = Duration::from_millis(500);
const OAUTH_WAIT_ATTEMPTS: usize = 20;

/// `/api/oauth/usage` 查询端点的本地缓存有效期：60s 内已有成功结果则直接复用 DB 数据，
/// 避免 UI 反复点击 / poller 同时打上游。
const USAGE_FRESH_TTL: Duration = Duration::from_secs(60);
/// `/api/oauth/usage` 收到 429 后的本地冷却时间：60s 内不再尝试上游。
const USAGE_429_COOLDOWN: Duration = Duration::from_secs(60);

pub struct AccountService {
    store: Arc<AccountStore>,
    cache: Arc<dyn CacheStore>,
    limit_store: Arc<LimitStore>,
    /// 账号级 `/api/oauth/usage` 429 冷却（in-memory，重启即清空，无需持久化）。
    usage_cooldown: Mutex<HashMap<i64, Instant>>,
    /// 用于 refresh_token 失效时的 session_key 自愈 fallback。
    /// Optional 是为了向后兼容（旧测试代码可能不传）。
    oauth_flow_svc: Option<Arc<crate::service::oauth_flow::OAuthFlowService>>,
    /// v1.9.16 B: sticky burst-evict 计数。
    /// 同一 session_hash 在短时间内被 preserve N 次后, 主动 evict 避免死锁。
    /// 场景: bot/agent 高 RPM 撞同一 sticky 号 + 该号被 Anthropic 短期限流 →
    /// 老逻辑 preserve 后下次还路由到这号 → 继续撞 429 → 形成死锁。
    /// 用 dashmap 避免锁竞争。
    sticky_burst_count: dashmap::DashMap<String, (u32, Instant)>,
}

impl AccountService {
    pub fn new(
        store: Arc<AccountStore>,
        cache: Arc<dyn CacheStore>,
        limit_store: Arc<LimitStore>,
    ) -> Self {
        Self {
            store,
            cache,
            limit_store,
            usage_cooldown: Mutex::new(HashMap::new()),
            oauth_flow_svc: None,
            sticky_burst_count: dashmap::DashMap::new(),
        }
    }

    /// 注入 OAuthFlowService，用于 refresh 失败时的 session_key 自愈。
    /// 在 main.rs 启动时调用。
    pub fn with_oauth_flow(
        mut self,
        oauth_flow_svc: Arc<crate::service::oauth_flow::OAuthFlowService>,
    ) -> Self {
        self.oauth_flow_svc = Some(oauth_flow_svc);
        self
    }

    /// 当前是否处于 429 冷却期。
    fn usage_in_cooldown(&self, id: i64) -> bool {
        let mut map = self.usage_cooldown.lock();
        match map.get(&id) {
            Some(until) if *until > Instant::now() => true,
            Some(_) => {
                map.remove(&id);
                false
            }
            None => false,
        }
    }

    fn mark_usage_cooldown(&self, id: i64) {
        let mut map = self.usage_cooldown.lock();
        map.insert(id, Instant::now() + USAGE_429_COOLDOWN);
    }

    /// 创建新账号并自动生成身份信息。
    pub async fn create_account(&self, a: &mut Account) -> Result<(), AppError> {
        let (device_id, env, prompt, process) =
            crate::model::identity::generate_canonical_identity();
        a.device_id = device_id;
        a.canonical_env = env;
        a.canonical_prompt = prompt;
        a.canonical_process = process;

        if a.status == crate::model::account::AccountStatus::Active
            && a.status.to_string() == "active"
        {
            // default already active
        }
        if a.concurrency == 0 {
            a.concurrency = 3;
        }
        if a.priority == 0 {
            a.priority = 50;
        }
        if a.billing_mode == crate::model::account::BillingMode::Strip
            && a.billing_mode.to_string() == "strip"
        {
            // default already strip
        }

        normalize_account_auth(a)?;

        self.store.create(a).await
    }

    pub async fn update_account(&self, a: &Account) -> Result<(), AppError> {
        let mut normalized = a.clone();
        normalize_account_auth(&mut normalized)?;
        self.store.update(&normalized).await
    }

    pub async fn delete_account(&self, id: i64) -> Result<(), AppError> {
        let r = self.store.delete(id).await;
        // 删账号同时清掉 LimitStore + metrics 里的 per-account 内存条目
        // (修 P2-N16: 防止删号后内存里幽灵账号持续累积)
        if r.is_ok() {
            self.limit_store.remove_account(id);
            crate::service::metrics::METRICS.remove_account(id);
        }
        r
    }

    /// 数据库连通性检查 (健康端点 /readyz 用)。
    pub async fn ping_db(&self) -> Result<(), AppError> {
        self.store.ping().await
    }

    pub async fn get_account(&self, id: i64) -> Result<Account, AppError> {
        self.store.get_by_id(id).await
    }

    pub async fn list_accounts(&self) -> Result<Vec<Account>, AppError> {
        self.store.list().await
    }

    pub async fn list_accounts_paged(
        &self,
        page: i64,
        page_size: i64,
    ) -> Result<(Vec<Account>, i64), AppError> {
        let total = self.store.count().await?;
        let accounts = self.store.list_paged(page, page_size).await?;
        Ok((accounts, total))
    }

    /// 使用粘性会话为请求选择账号。
    /// `exclude_ids` 为令牌的不可用账号，`allowed_ids` 为令牌的可用账号（空表示不限制）。
    pub async fn select_account(
        &self,
        session_hash: &str,
        exclude_ids: &[i64],
        allowed_ids: &[i64],
        skip_rate_limit_filter: bool,
    ) -> Result<Account, AppError> {
        // sticky 首选账号被临时限流时保留 sticky，本次走 fallback 选别的号；
        // 等首选恢复后下次请求能继续命中。仅在永久失效（账号 disabled / 被删 / 被 token 黑名单）时删 sticky。
        let mut sticky_preserved = false;

        // 检查粘性会话
        if !session_hash.is_empty() {
            if let Ok(Some(account_id)) = self.cache.get_session_account_id(session_hash).await {
                if account_id > 0 {
                    // Try the schedulable cache first; fall back to DB on miss.
                    let account_opt = match self.store.get_schedulable_cached(account_id).await {
                        Some(a) => Some(a),
                        None => self.store.get_by_id(account_id).await.ok(),
                    };
                    if let Some(account) = account_opt {
                        let id_allowed =
                            allowed_ids.is_empty() || allowed_ids.contains(&account_id);
                        let rate_limit_ok = if skip_rate_limit_filter {
                            self.limit_store.sonnet_available(account_id)
                        } else {
                            self.limit_store.availability(account_id).is_available()
                        };
                        let cooldown_ok = !account.is_in_403_cooldown();
                        if account.is_schedulable()
                            && !exclude_ids.contains(&account_id)
                            && id_allowed
                            && rate_limit_ok
                            && cooldown_ok
                        {
                            // v1.9.16 B: sticky 命中成功 → 清掉 burst 计数 (避免长期累积)
                            self.sticky_burst_count.remove(session_hash);
                            return Ok(account);
                        }
                        // 校验失败：判断是临时还是永久
                        // 账号本身正常（schedulable + 通过权限）但只是被限流 / 403 cooldown → 临时，保留 sticky
                        // 否则（账号挂了 / 被黑名单）→ 永久，删 sticky
                        if account.is_schedulable()
                            && id_allowed
                            && !exclude_ids.contains(&account_id)
                        {
                            // v1.9.16 B: sticky burst-evict 检测
                            // 同 session_hash 短时间内反复被 preserve = 客户高 RPM 持续撞限流号,
                            // 老逻辑保留 sticky 等号恢复 → 形成死锁 (bot 1 分钟撞几十次同号 429)。
                            // 阈值: 60s 内被 preserve >= 5 次 → 主动 evict sticky, 让下次请求选别的号。
                            const BURST_WINDOW_SECS: u64 = 60;
                            const BURST_THRESHOLD: u32 = 5;
                            let now_inst = Instant::now();
                            let mut should_evict_burst = false;
                            {
                                let mut entry = self
                                    .sticky_burst_count
                                    .entry(session_hash.to_string())
                                    .or_insert((0, now_inst));
                                let elapsed = now_inst.saturating_duration_since(entry.1);
                                if elapsed > Duration::from_secs(BURST_WINDOW_SECS) {
                                    // 窗口外, 重置
                                    *entry = (1, now_inst);
                                } else {
                                    entry.0 += 1;
                                    if entry.0 >= BURST_THRESHOLD {
                                        should_evict_burst = true;
                                    }
                                }
                            }
                            if should_evict_burst {
                                let _ = self.cache.delete_session(session_hash).await;
                                self.sticky_burst_count.remove(session_hash);
                                crate::service::metrics::METRICS.record_sticky_evicted();
                                warn!(
                                    "sticky burst-evict for account {} (preserved {}x in {}s, route to fresh account)",
                                    account_id, BURST_THRESHOLD, BURST_WINDOW_SECS
                                );
                                // fall through 走候选选号 (不进 sticky_preserved 分支)
                            } else {
                                sticky_preserved = true;
                                crate::service::metrics::METRICS.record_sticky_preserved();
                                // 启动后头 60s 内的 sticky-preserved 大概率是
                                // "Redis sticky 还在但内存 LimitStore 是空的" 引起,
                                // 单独计数方便观察重启抖动 (Phase2: 重启第一波监控)
                                let started = crate::service::metrics::METRICS
                                    .started_at_unix
                                    .load(std::sync::atomic::Ordering::Relaxed);
                                let now = chrono::Utc::now().timestamp();
                                if started > 0 && now - started < 60 {
                                    crate::service::metrics::METRICS
                                        .record_post_restart_first_select();
                                }
                                info!(
                                    "sticky preserved for account {} (rate-limited, will retry next request)",
                                    account_id
                                );
                            }
                        } else {
                            let _ = self.cache.delete_session(session_hash).await;
                            crate::service::metrics::METRICS.record_sticky_evicted();
                        }
                    } else {
                        // 账号查不到（被删了）→ 永久失效
                        let _ = self.cache.delete_session(session_hash).await;
                        crate::service::metrics::METRICS.record_sticky_evicted();
                    }
                }
            }
        }

        // 获取可调度账号
        let accounts = self.store.list_schedulable().await?;
        let total_schedulable = accounts.len();

        // 平台过滤: Claude 网关 (/v1/messages 等) 只调度 platform=claude 的账号
        // OpenAI 账号有独立的 /v1/chat/completions handler 处理, 不能混入 Claude 路径,
        // 否则会用 Anthropic 的 refresh endpoint 去刷 OpenAI RT, 拿到 403。
        // 兼容旧数据: platform 为空字符串视为 claude (DB DEFAULT 是 'claude')
        let mut limited_out: Vec<i64> = Vec::new();
        let accounts: Vec<Account> = accounts
            .into_iter()
            .filter(|a| a.platform.is_empty() || a.platform == "claude")
            .collect();
        let candidates: Vec<Account> = accounts
            .into_iter()
            .filter(|a| {
                if exclude_ids.contains(&a.id) {
                    return false;
                }
                if !(allowed_ids.is_empty() || allowed_ids.contains(&a.id)) {
                    return false;
                }
                // 软恢复 cooldown 检查 — 与 LimitStore 限流同等处理 (一并算 limited_out
                // 让上层 sticky 路径走 "preserved" 分支保留首选)
                if a.is_in_403_cooldown() {
                    limited_out.push(a.id);
                    return false;
                }
                if skip_rate_limit_filter {
                    if !self.limit_store.sonnet_available(a.id) {
                        limited_out.push(a.id);
                        return false;
                    }
                    return true;
                }
                let availability = self.limit_store.availability(a.id);
                if !availability.is_available() {
                    limited_out.push(a.id);
                    return false;
                }
                true
            })
            .collect();

        if !limited_out.is_empty() {
            info!(
                "select: {} of {} schedulable accounts filtered by limit state: {:?}",
                limited_out.len(),
                total_schedulable,
                limited_out
            );
        }

        if candidates.is_empty() {
            return Err(AppError::ServiceUnavailable("no available accounts".into()));
        }

        // 按优先级分组，同优先级内随机选择
        // effective_priority = a.priority + LimitStore 软降权 (90-97% util 时 +10)
        // 让接近满载的号自然落到次选, 健康号先消耗
        let limit_store = self.limit_store.clone();
        let selected = select_by_priority(&candidates, |a| {
            a.priority + limit_store.priority_penalty(a.id)
        });

        // 绑定粘性会话：仅在没有保留的 sticky 时才绑（否则会覆盖被保留的首选）
        if !session_hash.is_empty() && !sticky_preserved {
            let _ = self
                .cache
                .set_session_account_id(session_hash, selected.id, STICKY_SESSION_TTL)
                .await;
        }

        Ok(selected)
    }

    /// 选择一个 OpenAI 账号 (platform=="openai")。
    ///
    /// 镜像 [`Self::select_account`] 的粘性会话 + 优先级 + 黑白名单逻辑,但:
    ///   - 平台过滤改为 `platform == "openai"`
    ///   - cache key 已经由调用方通过 [`crate::service::codex_session::sticky_session_cache_key`]
    ///     加上 `openai:` 前缀,与 Claude 粘性会话隔离
    ///   - 不走 `LimitStore` (OpenAI 用量目前是异步落到 `account.extra` 的, 后续 PR 再联动调度)
    pub async fn select_openai_account(
        &self,
        session_hash: &str,
        exclude_ids: &[i64],
        allowed_ids: &[i64],
    ) -> Result<Account, AppError> {
        use crate::service::openai_limit::openai_schedulable;

        // sticky 首选账号被临时限流时保留 sticky；仅永久失效才删
        let mut sticky_preserved = false;

        // 1) 命中粘性会话
        if !session_hash.is_empty() {
            if let Ok(Some(account_id)) = self.cache.get_session_account_id(session_hash).await {
                if account_id > 0 {
                    if let Ok(account) = self.store.get_by_id(account_id).await {
                        let id_allowed =
                            allowed_ids.is_empty() || allowed_ids.contains(&account_id);
                        let scheduable = openai_schedulable(&account);
                        if account.platform == "openai"
                            && account.is_schedulable()
                            && !exclude_ids.contains(&account_id)
                            && id_allowed
                            && scheduable
                        {
                            return Ok(account);
                        }
                        // 平台正确 + 账号正常 + 通过权限 → 仅 codex 用量临时不可调度，保留 sticky
                        if account.platform == "openai"
                            && account.is_schedulable()
                            && id_allowed
                            && !exclude_ids.contains(&account_id)
                        {
                            sticky_preserved = true;
                            crate::service::metrics::METRICS.record_sticky_preserved();
                            info!(
                                "openai sticky preserved for account {} (codex usage / 429, will retry)",
                                account_id
                            );
                        } else {
                            let _ = self.cache.delete_session(session_hash).await;
                            crate::service::metrics::METRICS.record_sticky_evicted();
                        }
                    } else {
                        // 账号查不到 → 永久失效
                        let _ = self.cache.delete_session(session_hash).await;
                        crate::service::metrics::METRICS.record_sticky_evicted();
                    }
                }
            }
        }

        // 2) 列出全部可调度账号, 过滤平台 + 用量/429
        let accounts = self.store.list_schedulable().await?;
        let total = accounts.len();
        let mut limited_out: Vec<i64> = Vec::new();
        let candidates: Vec<Account> = accounts
            .into_iter()
            .filter(|a| a.platform == "openai")
            .filter(|a| {
                if exclude_ids.contains(&a.id) {
                    return false;
                }
                if !(allowed_ids.is_empty() || allowed_ids.contains(&a.id)) {
                    return false;
                }
                if !openai_schedulable(a) {
                    limited_out.push(a.id);
                    return false;
                }
                true
            })
            .collect();

        if !limited_out.is_empty() {
            crate::service::metrics::METRICS
                .record_filtered_by_limit(limited_out.len() as u64);
            info!(
                "select_openai: {} of {} schedulable accounts filtered by codex usage / 429: {:?}",
                limited_out.len(),
                total,
                limited_out
            );
        }

        if candidates.is_empty() {
            return Err(AppError::ServiceUnavailable(
                "no available openai accounts".into(),
            ));
        }

        // OpenAI 路径不走 LimitStore 软降权 (它有独立的用量门禁), 直接按 a.priority 选
        let selected = select_by_priority(&candidates, |a| a.priority);

        // 3) 写回粘性绑定（仅在没有保留的 sticky 时）
        if !session_hash.is_empty() && !sticky_preserved {
            let _ = self
                .cache
                .set_session_account_id(session_hash, selected.id, STICKY_SESSION_TTL)
                .await;
        }

        Ok(selected)
    }

    /// 尝试获取账号的并发槽位。
    pub async fn acquire_slot(&self, account_id: i64, max: i32) -> Result<bool, AppError> {
        let key = format!("concurrency:account:{}", account_id);
        self.cache
            .acquire_slot(&key, max, Duration::from_secs(300))
            .await
    }

    /// 释放并发槽位。
    pub async fn release_slot(&self, account_id: i64) {
        let key = format!("concurrency:account:{}", account_id);
        self.cache.release_slot(&key).await;
    }

    /// 读取账号当前并发占用（best-effort 快照，供管理界面显示）。
    /// 对外保证非负：Redis 过期键遇 DECR 会落到负数，这里统一 clamp 到 0。
    pub async fn peek_concurrency(&self, account_id: i64) -> i64 {
        let key = format!("concurrency:account:{}", account_id);
        self.cache.peek_slot(&key).await.max(0)
    }

    /// 构造一个绑定到该账号并发槽的 SlotHolder（不会自行获取；调用者需先 acquire_slot）。
    pub fn slot_holder_for(&self, account_id: i64) -> crate::service::gateway::SlotHolder {
        let key = format!("concurrency:account:{}", account_id);
        crate::service::gateway::SlotHolder::new(self.cache.clone(), key)
    }

    /// 暴露 cache 给 gateway (修 C1: token-level concurrency slot 用)。
    /// 不希望 gateway 直接持 cache 字段, 通过 account_svc 取避免循环依赖。
    pub fn cache(&self) -> &Arc<dyn CacheStore> {
        &self.cache
    }

    /// 从 Anthropic API 获取账号用量并缓存到数据库。
    /// 仅支持 OAuth 账号，SetupToken 账号无法查询用量。
    ///
    /// 限频策略：
    /// - DB 中 `usage_fetched_at` < 60s 的成功结果直接复用（覆盖 UI 反复点击 / poller 两个调用源）；
    /// - 上游回 429 后，账号进入 60s 本地冷却，期间所有调用直接返回 `TooManyRequests`，
    ///   避免持续打上游被滚雪球。
    /// 强制清除内存里的本地软限流标记（rate_limited_until / status=Rejected）。
    ///
    /// 使用场景：admin 发现账号 admin UI 显示用量已重置但 selector 仍持续过滤
    /// 该账号时（典型死锁：absorb_headers 写入的 status/until 没机会被新请求刷新），
    /// 通过 admin 按钮手动调用此方法。返回布尔表示是否真的清了什么。
    pub fn clear_limit_runtime_flags(&self, id: i64) -> bool {
        self.limit_store.clear_runtime_flags(id)
    }

    /// 当前内存里仍有效的短期限流截止时间 (UI 用, 倒计时显示)。
    pub fn peek_rate_limited_until(
        &self,
        id: i64,
    ) -> Option<chrono::DateTime<chrono::Utc>> {
        self.limit_store.peek_rate_limited_until(id)
    }

    /// 把账号映射到 5 类用户视角的状态。优先级（高 → 低）：
    /// 封禁 / 停用 > 失效 > 限流中 > 可用。返回的 reason 给 UI 展示用。
    pub fn categorize(&self, account: &Account) -> AccountCategorization {
        use crate::model::account::AccountStatus;

        // 1. status=disabled → 封禁 vs 停用 (按 disable_reason 文本区分)
        if account.status == AccountStatus::Disabled {
            let reason = account.disable_reason.as_str();
            // 与 gateway.rs 写入的字面量保持一致:
            //   - "403 认证失败"           (v1.8.x 旧)
            //   - "403 认证失败 (3次累计触顶)" (v1.9.7+ 软恢复触顶)
            //   - "组织已被封禁 (400)"      (v1.9.8+ 400 org disabled)
            // 这些都是上游主动封号 → Banned; 否则视为人工停用 → Stopped
            let lower = reason.to_lowercase();
            let is_banned = reason.contains("403")
                || reason.contains("400")
                || reason.contains("认证失败")
                || reason.contains("封禁")
                || lower.contains("violation")
                || lower.contains("forbidden")
                || lower.contains("disabled");
            if is_banned {
                return AccountCategorization {
                    category: AccountCategory::Banned,
                    reason: if reason.is_empty() {
                        "上游已封禁".into()
                    } else {
                        reason.to_string()
                    },
                    recovers_at: None,
                };
            }
            return AccountCategorization {
                category: AccountCategory::Stopped,
                reason: if reason.is_empty() {
                    "已停用".into()
                } else {
                    reason.to_string()
                },
                recovers_at: None,
            };
        }

        // 2. status=error 在当前实现里几乎不写,但为完整性保留——视作失效。
        if account.status == AccountStatus::Error {
            return AccountCategorization {
                category: AccountCategory::Invalid,
                reason: if account.auth_error.is_empty() {
                    "状态异常".into()
                } else {
                    account.auth_error.clone()
                },
                recovers_at: None,
            };
        }

        // status=active 之后的判定:
        // 3. auth_error 非空 → 失效 (token 失效, 需人工处理)
        if !account.auth_error.is_empty() {
            return AccountCategorization {
                category: AccountCategory::Invalid,
                reason: account.auth_error.clone(),
                recovers_at: None,
            };
        }

        // 3b. 软恢复 403 cooldown 中 → 归 RateLimited (调度器已经跳过, UI 也要显示成
        // "限流中" 而非 "可用", 否则用户看到 status=active 误以为正常)。
        if account.is_in_403_cooldown() {
            let recovers_at = account
                .extra
                .get("auth_403_cooldown_until")
                .and_then(|v| v.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc));
            let count = account
                .extra
                .get("auth_403_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            return AccountCategorization {
                category: AccountCategory::RateLimited,
                reason: format!("403 cooldown ({}/3 strikes)", count),
                recovers_at,
            };
        }

        // 4. LimitStore 内存判定 → 限流中
        match self.limit_store.availability(account.id) {
            crate::service::limit::Availability::Unavailable { reason, until } => {
                AccountCategorization {
                    category: AccountCategory::RateLimited,
                    reason,
                    recovers_at: until,
                }
            }
            crate::service::limit::Availability::Available => {
                // 4b. 内存空 → 回退看 DB usage_data (启动后 hydrate 失败 / 老数据等场景兜底)
                if let Some(cat) = categorize_from_db_usage(&account.usage_data) {
                    return cat;
                }
                // 5. 全部检查通过
                AccountCategorization {
                    category: AccountCategory::Available,
                    reason: String::new(),
                    recovers_at: None,
                }
            }
        }
    }

    pub async fn refresh_usage(&self, id: i64) -> Result<serde_json::Value, AppError> {
        let account = self.store.get_by_id(id).await?;
        if account.auth_type != crate::model::account::AccountAuthType::Oauth {
            return Err(AppError::BadRequest(
                "长效 Token 账号不支持查询用量（仅 OAuth 账号可用）".into(),
            ));
        }

        // 1) 60s 内有成功查询 → 直接复用 DB 数据，不打上游。
        if let Some(fetched_at) = account.usage_fetched_at {
            let age = Utc::now().signed_duration_since(fetched_at);
            if age.num_seconds() >= 0
                && age.to_std().map(|d| d < USAGE_FRESH_TTL).unwrap_or(false)
            {
                info!(
                    "refresh_usage: account {} → cache hit (age={}s, ttl=60s)",
                    id,
                    age.num_seconds()
                );
                // 即使走 cache hit,也把缓存数据再 ingest 一次内存——确保自动死锁保护
                // (auto_clear_stale_runtime_flags) 有机会运行。否则用户在死锁状态下
                // 60s 内连点"刷新用量"按钮全部走 cache 直返,内存陈旧标记永远清不掉。
                self.limit_store.ingest_usage_json(id, &account.usage_data);
                return Ok(account.usage_data.clone());
            }
        }

        // 2) 60s 内被上游 429 过 → 直接返回 Err，跳过上游。
        if self.usage_in_cooldown(id) {
            info!(
                "refresh_usage: account {} → in 60s 429 cooldown, skipping upstream",
                id
            );
            return Err(AppError::TooManyRequests(
                "usage query in 60s local cooldown after recent 429".into(),
            ));
        }

        info!(
            "refresh_usage: account {} → fetching upstream /api/oauth/usage",
            id
        );
        let token = self.resolve_oauth_access_token(&account).await?;
        match crate::service::oauth::fetch_usage(&token, &account.proxy_url).await {
            Ok(usage) => {
                let usage_str = serde_json::to_string(&usage).unwrap_or_else(|_| "{}".into());
                self.store.update_usage(id, &usage_str).await?;
                // 同步 LimitStore 内存热态，避免前端手动刷新数据与响应头数据互相覆盖
                self.limit_store.ingest_usage_json(id, &usage);
                info!("refresh_usage: account {} → upstream OK, cached", id);
                Ok(usage)
            }
            Err(e) => {
                if matches!(e, AppError::TooManyRequests(_)) {
                    self.mark_usage_cooldown(id);
                    warn!(
                        "refresh_usage: account {} → upstream 429, cooldown for 60s: {}",
                        id, e
                    );
                } else {
                    warn!("refresh_usage: account {} → upstream error: {}", id, e);
                }
                Err(e)
            }
        }
    }

    pub async fn resolve_upstream_token(&self, id: i64) -> Result<String, AppError> {
        let account = self.store.get_by_id(id).await?;
        self.resolve_upstream_token_with(&account).await
    }

    /// Same as `resolve_upstream_token` but reuses an already-fetched `Account`,
    /// avoiding a redundant `get_by_id` round-trip. The refresh path still
    /// re-reads fresh data internally, so stale local fields are safe.
    pub async fn resolve_upstream_token_with(
        &self,
        account: &Account,
    ) -> Result<String, AppError> {
        match account.auth_type {
            AccountAuthType::SetupToken => {
                if account.setup_token.is_empty() {
                    return Err(AppError::ServiceUnavailable("setup token is empty".into()));
                }
                Ok(account.setup_token.clone())
            }
            AccountAuthType::Oauth => self.resolve_oauth_access_token(account).await,
        }
    }

    async fn resolve_oauth_access_token(&self, account: &Account) -> Result<String, AppError> {
        if account.has_valid_oauth_access_token(OAUTH_REFRESH_BUFFER_SECONDS) {
            return Ok(account.access_token.clone());
        }
        if account.refresh_token.is_empty() {
            let _ = self
                .store
                .update_auth_error(account.id, "missing refresh token")
                .await;
            return Err(AppError::ServiceUnavailable(
                "oauth refresh token is empty".into(),
            ));
        }

        let lock_key = format!("oauth:refresh:account:{}", account.id);
        let lock_owner = Uuid::new_v4().to_string();
        let acquired = self
            .cache
            .acquire_lock(&lock_key, &lock_owner, OAUTH_LOCK_TTL)
            .await?;

        if acquired {
            let result = self.refresh_oauth_access_token(account.id).await;
            self.cache.release_lock(&lock_key, &lock_owner).await;
            return result;
        }

        for _ in 0..OAUTH_WAIT_ATTEMPTS {
            sleep(OAUTH_WAIT_RETRY).await;
            let latest = self.store.get_by_id(account.id).await?;
            if latest.has_valid_oauth_access_token(OAUTH_REFRESH_BUFFER_SECONDS) {
                return Ok(latest.access_token);
            }
        }

        Err(AppError::ServiceUnavailable(
            "oauth token refresh timeout".into(),
        ))
    }

    async fn refresh_oauth_access_token(&self, id: i64) -> Result<String, AppError> {
        let latest = self.store.get_by_id(id).await?;
        if latest.has_valid_oauth_access_token(OAUTH_REFRESH_BUFFER_SECONDS) {
            return Ok(latest.access_token);
        }
        if latest.refresh_token.is_empty() {
            let _ = self
                .store
                .update_auth_error(id, "missing refresh token")
                .await;
            return Err(AppError::ServiceUnavailable(
                "oauth refresh token is empty".into(),
            ));
        }

        let fallback_access_token = latest.access_token.clone();
        let fallback_is_still_valid = latest
            .expires_at
            .map(|expires_at| expires_at > Utc::now())
            .unwrap_or(false);

        match crate::service::oauth::refresh_oauth_token(&latest.refresh_token, &latest.proxy_url)
            .await
        {
            Ok(tokens) => {
                self.store
                    .update_oauth_tokens(
                        id,
                        &tokens.access_token,
                        &tokens.refresh_token,
                        tokens.expires_at,
                    )
                    .await?;
                crate::service::metrics::METRICS
                    .record_oauth_refresh(crate::service::metrics::Platform::Claude, true);
                Ok(tokens.access_token)
            }
            Err(err) => {
                crate::service::metrics::METRICS
                    .record_oauth_refresh(crate::service::metrics::Platform::Claude, false);
                let msg = err.to_string();

                // 🆕 兜底 1: refresh 挂了但 session_key 还在 → 用 session_key 重新走 cookie_auth
                // 这是"账号自愈"机制：refresh_token 被吊销 / 过期 → 不需要人工重导
                if !latest.session_key.is_empty() {
                    if let Some(ref oauth_flow) = self.oauth_flow_svc {
                        warn!(
                            "oauth refresh failed for account {}, trying session_key fallback: {}",
                            id, msg
                        );
                        let req = crate::service::oauth_flow::CookieAuthRequest {
                            session_key: latest.session_key.clone(),
                            proxy_url: if latest.proxy_url.is_empty() {
                                None
                            } else {
                                Some(latest.proxy_url.clone())
                            },
                            scope: None,
                        };
                        match oauth_flow.cookie_auth(&req).await {
                            Ok(token) => {
                                let expires_at = if token.expires_at > 0 {
                                    chrono::TimeZone::timestamp_opt(&Utc, token.expires_at, 0)
                                        .single()
                                        .unwrap_or_else(|| {
                                            Utc::now() + chrono::Duration::seconds(token.expires_in)
                                        })
                                } else {
                                    Utc::now() + chrono::Duration::seconds(token.expires_in)
                                };
                                if let Err(e) = self
                                    .store
                                    .update_oauth_tokens(
                                        id,
                                        &token.access_token,
                                        &token.refresh_token,
                                        expires_at,
                                    )
                                    .await
                                {
                                    warn!(
                                        "session_key recovery succeeded but DB update failed for account {}: {}",
                                        id, e
                                    );
                                }
                                crate::service::metrics::METRICS
                                    .record_oauth_recovery_session_key(true);
                                let count = crate::service::metrics::METRICS
                                    .record_oauth_fallback_for_account(id);
                                if count >= 3 {
                                    warn!(
                                        "oauth fallback for account {} hit {} times — session_key may be stale, re-import recommended",
                                        id, count
                                    );
                                }
                                info!(
                                    "oauth recovered via session_key for account {} (refresh_token was {})",
                                    id,
                                    if msg.is_empty() { "invalid" } else { msg.as_str() }
                                );
                                let _ = self.store.update_auth_error(id, "").await;
                                return Ok(token.access_token);
                            }
                            Err(e) => {
                                crate::service::metrics::METRICS
                                    .record_oauth_recovery_session_key(false);
                                warn!(
                                    "session_key recovery also failed for account {}: {}",
                                    id, e
                                );
                                // fall through to next fallback
                            }
                        }
                    }
                }

                // 错误归因 (N2 修复):
                //   AppError::BadRequest = 上游明确拒绝 (4xx, refresh_token 真的废了) → 写 auth_error
                //   AppError::Internal / TooManyRequests = 上游抖动 (5xx/网络/429) → 不写 auth_error,
                //   避免 dashboard 把临时错误显示成账号失效, 误导运维手动停号
                let is_permanent = matches!(err, AppError::BadRequest(_));
                if is_permanent {
                    let _ = self.store.update_auth_error(id, &msg).await;
                } else {
                    warn!(
                        "oauth refresh transient error for account {} (not marking auth_error): {}",
                        id, msg
                    );
                }

                if fallback_is_still_valid && !fallback_access_token.is_empty() {
                    warn!(
                        "oauth refresh failed for account {}, using current access token until expiry: {}",
                        id, msg
                    );
                    // fallback 旧 token 还有效 → 把可能误标的 auth_error 清掉,
                    // 因为账号实际上是好的, 只是上游抖了一下
                    if !is_permanent {
                        let _ = self.store.update_auth_error(id, "").await;
                    }
                    return Ok(fallback_access_token);
                }
                Err(AppError::ServiceUnavailable(format!(
                    "oauth refresh failed: {}",
                    msg
                )))
            }
        }
    }

    pub async fn set_rate_limit(
        &self,
        id: i64,
        reset_at: chrono::DateTime<Utc>,
    ) -> Result<(), AppError> {
        self.store.set_rate_limit(id, reset_at).await
    }

    pub async fn disable_account(
        &self,
        id: i64,
        status: crate::model::account::AccountStatus,
        reason: &str,
        rate_limit_reset_at: Option<chrono::DateTime<Utc>>,
    ) -> Result<(), AppError> {
        let r = self
            .store
            .disable_account(id, status, reason, rate_limit_reset_at)
            .await;
        // 修 E1: 账号被 disable 时上报 metric, 让 prometheus 能告警
        // "短时间内 disable 数量增量 > 阈值" (例如 5min 内 ≥3 个号挂掉 → 报警)
        if r.is_ok() {
            // reason 多种多样 (403 认证失败 / oauth invalid_grant / 手动停用 ...);
            // 归类成有限几个标签防 cardinality 爆炸
            let reason_label = classify_disable_reason(reason);
            crate::service::metrics::METRICS.record_account_disabled(reason_label);
        }
        r
    }

    pub async fn enable_account(&self, id: i64) -> Result<(), AppError> {
        self.store.enable_account(id).await
    }

    /// 上游 403 软恢复 — 累积计数 + 短期 cooldown,达阈值才永久 disable。
    ///
    /// 触发位置: `gateway.rs` 收到上游 403 时调用 (替代直接 disable_account)。
    /// 状态字段全塞 `account.extra` JSONB,无 schema 改动。
    ///
    /// 行为:
    /// - 拉账号当前 extra,读 `auth_403_first_at` / `auth_403_count`
    /// - 7d 滚动窗口外 → 重置 first_at, count = 1
    /// - 窗口内 → count += 1
    /// - count < threshold → 写 cooldown_until = now + COOLDOWN_SECS, 调度器跳过
    /// - count >= threshold → 调用现有 `disable_account` 永久 disable (行为不变)
    ///
    /// env 调参 (默认值匹配 Anthropic 组织级临时风控通常 24h 才解的实际经验):
    /// - `CCBRIDGE_403_COOLDOWN_SECS` 默认 86400 (24h)
    /// - `CCBRIDGE_403_STRIKE_THRESHOLD` 默认 3
    /// - `CCBRIDGE_403_WINDOW_DAYS` 默认 7
    pub async fn record_403(&self, account_id: i64) -> Result<(), AppError> {
        let cooldown_secs: i64 = std::env::var("CCBRIDGE_403_COOLDOWN_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|n: &i64| *n > 0)
            .unwrap_or(86400);
        let threshold: u64 = std::env::var("CCBRIDGE_403_STRIKE_THRESHOLD")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|n: &u64| *n > 0)
            .unwrap_or(3);
        let window_days: i64 = std::env::var("CCBRIDGE_403_WINDOW_DAYS")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|n: &i64| *n > 0)
            .unwrap_or(7);

        let account = self.store.get_by_id(account_id).await?;
        let now = Utc::now();

        let mut extra = account
            .extra
            .as_object()
            .cloned()
            .unwrap_or_default();

        // 7d 滚动窗判断: first_at 超出窗口或没有 → 视为新一轮,重置
        let first_at = extra
            .get("auth_403_first_at")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        let in_window = first_at
            .map(|t| (now - t).num_days() < window_days)
            .unwrap_or(false);

        let count: u64 = if in_window {
            extra
                .get("auth_403_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                + 1
        } else {
            extra.insert(
                "auth_403_first_at".into(),
                serde_json::Value::String(now.to_rfc3339()),
            );
            1
        };
        extra.insert(
            "auth_403_count".into(),
            serde_json::Value::Number(count.into()),
        );

        if count >= threshold {
            // 触顶 → 永久 disable, 走现有路径 (含 metrics + cache invalidate)
            // 顺带把 cooldown 字段清掉, disabled 状态本身已经使账号不可调度
            extra.remove("auth_403_cooldown_until");
            // 先把 extra 落 (含累积计数, 便于运维看历史)
            let _ = self
                .store
                .update_extra(account_id, serde_json::Value::Object(extra))
                .await;
            self.disable_account(
                account_id,
                crate::model::account::AccountStatus::Disabled,
                &format!("403 认证失败 ({}次累计触顶)", count),
                None,
            )
            .await?;
            warn!(
                "account {} → 403 strike {}/{} → permanently disabled",
                account_id, count, threshold
            );
        } else {
            let until = now + chrono::Duration::seconds(cooldown_secs);
            extra.insert(
                "auth_403_cooldown_until".into(),
                serde_json::Value::String(until.to_rfc3339()),
            );
            self.store
                .update_extra(account_id, serde_json::Value::Object(extra))
                .await?;
            crate::service::metrics::METRICS.record_403_cooldown_set();
            info!(
                "account {} → 403 strike {}/{} → cooldown {}s (until {})",
                account_id, count, threshold, cooldown_secs, until
            );
        }
        Ok(())
    }

    /// 成功响应路径调用 — 清空 403 计数 + cooldown 字段。
    /// 调用方应该先用 `account.has_403_strikes()` 判断,避免无意义的 DB 写。
    pub async fn clear_403_state(&self, account_id: i64) -> Result<(), AppError> {
        let account = self.store.get_by_id(account_id).await?;
        let mut extra = match account.extra.as_object() {
            Some(o)
                if o.contains_key("auth_403_count")
                    || o.contains_key("auth_403_cooldown_until")
                    || o.contains_key("auth_403_first_at") =>
            {
                o.clone()
            }
            _ => return Ok(()),
        };
        extra.remove("auth_403_count");
        extra.remove("auth_403_first_at");
        extra.remove("auth_403_cooldown_until");
        self.store
            .update_extra(account_id, serde_json::Value::Object(extra))
            .await?;
        crate::service::metrics::METRICS.record_403_recovery();
        info!("account {} → 403 state cleared (success after strikes)", account_id);
        Ok(())
    }

    /// 获取 OpenAI OAuth 账号最新 access_token,带全局刷新锁防 thundering herd。
    ///
    /// 与 [`Self::resolve_oauth_access_token`] 同样的 lock + 等待 pattern, 区别:
    /// - 调用 `OpenAIOAuthService::refresh_token` (而非 Claude oauth)
    /// - 不存在 SetupToken 路径 (调用方自己判断 auth_type)
    /// - 持久化时同时更新 extra (refresh 可能拿到新的 chatgpt_account_id 等)
    ///
    /// 返回刷新后的 Account (含最新 access_token / refresh_token / expires_at)。
    pub async fn resolve_openai_access_token(
        &self,
        account: &Account,
        openai_oauth: &crate::service::openai_oauth::OpenAIOAuthService,
    ) -> Result<Account, AppError> {
        if account.has_valid_oauth_access_token(OAUTH_REFRESH_BUFFER_SECONDS) {
            return Ok(account.clone());
        }
        if account.refresh_token.is_empty() {
            return Err(AppError::ServiceUnavailable(
                "openai refresh token is empty".into(),
            ));
        }

        let lock_key = format!("oauth:refresh:account:{}", account.id);
        let lock_owner = Uuid::new_v4().to_string();
        let acquired = self
            .cache
            .acquire_lock(&lock_key, &lock_owner, OAUTH_LOCK_TTL)
            .await?;

        if acquired {
            let result = self
                .do_openai_refresh(account.id, openai_oauth)
                .await;
            self.cache.release_lock(&lock_key, &lock_owner).await;
            return result;
        }

        for _ in 0..OAUTH_WAIT_ATTEMPTS {
            sleep(OAUTH_WAIT_RETRY).await;
            let latest = self.store.get_by_id(account.id).await?;
            if latest.has_valid_oauth_access_token(OAUTH_REFRESH_BUFFER_SECONDS) {
                return Ok(latest);
            }
        }
        Err(AppError::ServiceUnavailable(
            "openai oauth token refresh timeout".into(),
        ))
    }

    async fn do_openai_refresh(
        &self,
        id: i64,
        openai_oauth: &crate::service::openai_oauth::OpenAIOAuthService,
    ) -> Result<Account, AppError> {
        let mut latest = self.store.get_by_id(id).await?;
        // double-check: 别人可能已经刷过了
        if latest.has_valid_oauth_access_token(OAUTH_REFRESH_BUFFER_SECONDS) {
            return Ok(latest);
        }
        if latest.refresh_token.is_empty() {
            return Err(AppError::ServiceUnavailable(
                "openai refresh token is empty".into(),
            ));
        }

        match openai_oauth
            .refresh_token(&latest.refresh_token, &latest.proxy_url)
            .await
        {
            Ok(r) => {
                latest.access_token = r.access_token.clone();
                if !r.refresh_token.is_empty() {
                    latest.refresh_token = r.refresh_token.clone();
                }
                if let Some(t) = chrono::TimeZone::timestamp_opt(&Utc, r.expires_at, 0).single() {
                    latest.expires_at = Some(t);
                }
                // 把 enrich 字段更新到 extra (refresh 可能拿到新 plan_type / org)
                if !r.chatgpt_account_id.is_empty()
                    || !r.organization_id.is_empty()
                    || !r.plan_type.is_empty()
                {
                    let mut extra = match latest.extra.as_object() {
                        Some(o) => o.clone(),
                        None => serde_json::Map::new(),
                    };
                    if !r.chatgpt_account_id.is_empty() {
                        extra.insert(
                            "chatgpt_account_id".into(),
                            serde_json::json!(r.chatgpt_account_id),
                        );
                    }
                    if !r.organization_id.is_empty() {
                        extra.insert(
                            "organization_id".into(),
                            serde_json::json!(r.organization_id),
                        );
                    }
                    if !r.plan_type.is_empty() {
                        extra.insert("plan_type".into(), serde_json::json!(r.plan_type));
                    }
                    latest.extra = serde_json::Value::Object(extra);
                }
                self.update_account(&latest).await?;
                Ok(latest)
            }
            Err(e) => {
                let msg = e.to_string();
                let _ = self.store.update_auth_error(id, &msg).await;
                // 如果当前 access_token 还能撑一会就先用旧的
                if !latest.access_token.is_empty()
                    && latest.expires_at.map(|t| t > Utc::now()).unwrap_or(false)
                {
                    warn!(
                        "openai refresh failed for account {}, using current access token until expiry: {}",
                        id, msg
                    );
                    return Ok(latest);
                }
                Err(AppError::ServiceUnavailable(format!(
                    "openai refresh failed: {}",
                    msg
                )))
            }
        }
    }
}

fn normalize_account_auth(account: &mut Account) -> Result<(), AppError> {
    match account.auth_type {
        AccountAuthType::SetupToken => {
            if account.setup_token.trim().is_empty() {
                return Err(AppError::BadRequest("setup_token is required".into()));
            }
            account.access_token.clear();
            account.refresh_token.clear();
            account.expires_at = None;
            account.oauth_refreshed_at = None;
            account.auth_error.clear();
        }
        AccountAuthType::Oauth => {
            if account.refresh_token.trim().is_empty() {
                return Err(AppError::BadRequest("refresh_token is required".into()));
            }
            account.setup_token.clear();
            account.auth_error.clear();
            if account.access_token.trim().is_empty() {
                account.access_token.clear();
                account.expires_at = None;
            }
        }
    }
    Ok(())
}

/// DB usage_data fallback 判定: LimitStore 内存空时, 仍然能从 DB 读出 5h/7d
/// utilization 来分类。返回 Some(限流分类) 或 None(健康/无数据视为可用)。
fn categorize_from_db_usage(usage: &serde_json::Value) -> Option<AccountCategorization> {
    let obj = usage.as_object()?;
    let now = chrono::Utc::now();

    fn read_window(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<(f64, chrono::DateTime<chrono::Utc>)> {
        let w = obj.get(key)?.as_object()?;
        // utilization 在 DB 里是 0-100 刻度 (build_usage_json 写入时乘了 100)
        let pct = w.get("utilization").and_then(|v| v.as_f64())?;
        let resets = w.get("resets_at").and_then(|v| v.as_str())?;
        let t = chrono::DateTime::parse_from_rfc3339(resets).ok()?.with_timezone(&chrono::Utc);
        Some((pct / 100.0, t))
    }

    // 命中阈值: utilization 必须达到 100% 且 resets_at 在未来
    if let Some((u, t)) = read_window(obj, "five_hour") {
        if u >= 1.0 && t > now {
            return Some(AccountCategorization {
                category: AccountCategory::RateLimited,
                reason: format!("5 小时窗口已用 {:.1}%", u * 100.0),
                recovers_at: Some(t),
            });
        }
    }
    if let Some((u, t)) = read_window(obj, "seven_day") {
        if u >= 1.0 && t > now {
            return Some(AccountCategorization {
                category: AccountCategory::RateLimited,
                reason: format!("7 天窗口已用 {:.1}%", u * 100.0),
                recovers_at: Some(t),
            });
        }
    }
    None
}

/// 根据客户端类型创建会话哈希。
/// CC 客户端：使用 metadata.user_id 中的 session_id。
/// API 客户端：使用 sha256(UA + 系统提示词/首条消息)。
/// 会话粘滞时长统一由 CacheStore TTL（24h）决定，不再在哈希键中嵌入小时窗口，
/// 否则会把实际 sticky 时长截断到 1 小时，并在跨小时边界引入上游账号抖动。
///
/// **多租户隔离 (修 N3)**: 同一 session_id / 同一 (UA, content) 在不同 api_token
/// 下应该路由到独立的 sticky, 否则会出现"用户 A 的 sticky 被用户 B 命中并失效"
/// 的串号 bug。`api_token_id` 作为 namespace 前缀混入 hash 输入。
/// 老调用方 (没有 token_id) 传 None 等价于 0, 与历史哈希值不再兼容 — 重启时
/// 现有 sticky 会失效一次, 短期内缓存命中率下跌, 几分钟内自然恢复。
pub fn generate_session_hash(
    user_agent: &str,
    body: &serde_json::Value,
    client_type: ClientType,
    api_token_id: Option<i64>,
) -> String {
    let ns = api_token_id.unwrap_or(0);
    if client_type == ClientType::ClaudeCode {
        if let Some(metadata) = body.get("metadata").and_then(|m| m.as_object()) {
            if let Some(user_id_str) = metadata.get("user_id").and_then(|u| u.as_str()) {
                // JSON 格式
                if let Ok(uid) = serde_json::from_str::<serde_json::Value>(user_id_str) {
                    if let Some(sid) = uid.get("session_id").and_then(|s| s.as_str()) {
                        if !sid.is_empty() {
                            // 加 token namespace 前缀, 防止 token_A 和 token_B
                            // 用了相同 session_id 时被路由到同一账号 sticky
                            return format!("k{}:{}", ns, sid);
                        }
                    }
                }
                // 旧格式
                if let Some(idx) = user_id_str.rfind("_session_") {
                    return format!("k{}:{}", ns, &user_id_str[idx + 9..]);
                }
            }
        }
    }

    // API 模式：UA + 系统提示词/首条消息 + 小时窗口
    let mut content = String::new();

    // Try system prompt first
    match body.get("system") {
        Some(serde_json::Value::String(sys)) => {
            content = sys.clone();
        }
        Some(serde_json::Value::Array(arr)) => {
            for item in arr {
                if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                    content = text.to_string();
                    break;
                }
            }
        }
        _ => {}
    }

    // 回退到首条消息
    if content.is_empty() {
        if let Some(messages) = body.get("messages").and_then(|m| m.as_array()) {
            if let Some(msg) = messages.first().and_then(|m| m.as_object()) {
                match msg.get("content") {
                    Some(serde_json::Value::String(c)) => {
                        content = c.clone();
                    }
                    Some(serde_json::Value::Array(arr)) => {
                        for item in arr {
                            if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                                content = text.to_string();
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    let raw = format!("k{}|{}|{}", ns, user_agent, content);
    let hash = Sha256::digest(raw.as_bytes());
    hex::encode(&hash[..16])
}

/// 按优先级选号: 入参 `effective_priority(a)` 给定每个账号的"实际"优先级,
/// 调用方可借此对接近满载 (90-97%) 的号叠加 SOFT_DEPRIORITIZE_PENALTY=10,
/// 让健康号自然先被选中。
///
/// 优先级数值越小越优先; 同 effective_priority 内随机选一个 (避免热点)。
fn select_by_priority<F: Fn(&Account) -> i32>(accounts: &[Account], effective_priority: F) -> Account {
    if accounts.len() == 1 {
        return accounts[0].clone();
    }

    // 找到最高 effective 优先级（最小数值）
    let best_priority = accounts.iter().map(&effective_priority).min().unwrap_or(50);

    // 收集相同 effective 优先级的所有账号
    let best: Vec<&Account> = accounts
        .iter()
        .filter(|a| effective_priority(a) == best_priority)
        .collect();

    // 同优先级内随机选择
    let idx = rand::thread_rng().gen_range(0..best.len());
    best[idx].clone()
}

/// 把 disable_account 的自由文本 reason 归类成有限几个 metric 标签,
/// 防止 prometheus label cardinality 爆炸 (修 E1)。
///
/// 用例:
///   - "403 认证失败"          → "auth_403"
///   - "oauth invalid_grant"   → "oauth_invalid_grant"
///   - "手动停用 via admin UI" → "manual"
///   - "上游 violation"        → "upstream_violation"
///   - 其它                    → "other"
pub fn classify_disable_reason(reason: &str) -> &'static str {
    let lower = reason.to_ascii_lowercase();
    if lower.contains("403") || lower.contains("认证") || lower.contains("auth") {
        "auth_403"
    } else if lower.contains("invalid_grant") || lower.contains("invalid grant") {
        "oauth_invalid_grant"
    } else if lower.contains("invalid_request") {
        "oauth_invalid_request"
    } else if lower.contains("violation") || lower.contains("封禁") || lower.contains("ban") {
        "upstream_violation"
    } else if lower.contains("手动") || lower.contains("manual") || lower.contains("admin") {
        "manual"
    } else if lower.contains("revoke") || lower.contains("revoked") {
        "token_revoked"
    } else {
        "other"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- generate_session_hash ----

    #[test]
    fn session_hash_is_deterministic_for_same_input() {
        let ua = "claude-cli/2.1.81 (external, cli)";
        let body = json!({
            "system": "You are Claude Code",
            "messages": [{"role": "user", "content": "hello"}],
        });
        let h1 = generate_session_hash(ua, &body, ClientType::API, None);
        let h2 = generate_session_hash(ua, &body, ClientType::API, None);
        assert_eq!(
            h1, h2,
            "same (ua, content) must yield identical hash — sticky TTL depends on this"
        );
        assert_eq!(h1.len(), 32, "hex of 16-byte prefix should be 32 chars");
    }

    #[test]
    fn session_hash_differs_by_system_prompt() {
        let ua = "claude-cli/2.1.81";
        let a = json!({"system": "prompt-A", "messages": [{"role": "user", "content": "x"}]});
        let b = json!({"system": "prompt-B", "messages": [{"role": "user", "content": "x"}]});
        assert_ne!(
            generate_session_hash(ua, &a, ClientType::API, None),
            generate_session_hash(ua, &b, ClientType::API, None)
        );
    }

    #[test]
    fn session_hash_falls_back_to_first_message_when_no_system() {
        let ua = "claude-cli/2.1.81";
        let a = json!({"messages": [{"role": "user", "content": "alpha"}]});
        let b = json!({"messages": [{"role": "user", "content": "beta"}]});
        let ha = generate_session_hash(ua, &a, ClientType::API, None);
        let hb = generate_session_hash(ua, &b, ClientType::API, None);
        assert_ne!(ha, hb);
    }

    #[test]
    fn session_hash_no_longer_embeds_hour_window() {
        // 回归测试：哈希不应在任何形式上依赖当前时间。
        // 以前的实现把 Utc::now().format("%Y-%m-%dT%H") 拼到原文里，使 sticky TTL 被截断到 1 小时。
        let ua = "claude-cli/2.1.81";
        let body = json!({
            "system": "stable-prompt",
            "messages": [{"role": "user", "content": "hi"}],
        });
        // 哈希在极短时间内多次调用必须相同（这是显而易见的，但如果再次引入 Utc::now()，跨小时会翻车）。
        let mut seen = std::collections::HashSet::new();
        for _ in 0..10 {
            seen.insert(generate_session_hash(ua, &body, ClientType::API, None));
        }
        assert_eq!(
            seen.len(),
            1,
            "hash must be pure function of (ua, content); any time dependency is a regression"
        );

        // 进一步：已知等价输入的哈希必须等于 sha256(k{ns}|ua|content) 前 16 字节 hex。
        // (修 N3: hash 加 api_token_id 命名空间, ns=0 表示无 token / 历史路径)
        let expected = {
            let raw = format!("k0|{}|{}", ua, "stable-prompt");
            let digest = Sha256::digest(raw.as_bytes());
            hex::encode(&digest[..16])
        };
        assert_eq!(
            generate_session_hash(ua, &body, ClientType::API, None),
            expected,
            "hash formula must be exactly sha256(k{{ns}}|ua|content)[..16]"
        );
    }

    #[test]
    fn session_hash_cc_mode_uses_session_id_from_metadata() {
        let ua = "claude-cli/2.1.81";
        let body = json!({
            "metadata": {
                "user_id": "{\"session_id\":\"sess-abc-123\",\"account_id\":\"xyz\"}"
            }
        });
        // 修 N3: ClaudeCode 模式也要带 k{ns}: 前缀 防多租户串号
        let h = generate_session_hash(ua, &body, ClientType::ClaudeCode, None);
        assert_eq!(h, "k0:sess-abc-123");
    }

    #[test]
    fn session_hash_isolated_across_api_tokens() {
        // 关键回归测试 (修 N3): 相同 (ua, body) 不同 api_token_id 必须产生不同 hash,
        // 否则 user_A 和 user_B 用相同 session_id 时 sticky 串号
        let ua = "claude-cli/2.1.81";
        let body = json!({
            "metadata": { "user_id": "{\"session_id\":\"shared-id\"}" }
        });
        let h_token1 = generate_session_hash(ua, &body, ClientType::ClaudeCode, Some(1));
        let h_token2 = generate_session_hash(ua, &body, ClientType::ClaudeCode, Some(2));
        let h_none = generate_session_hash(ua, &body, ClientType::ClaudeCode, None);
        assert_ne!(h_token1, h_token2, "ClaudeCode: 不同 token_id 必须独立 sticky");
        assert_ne!(h_token1, h_none);
        assert_eq!(h_none, "k0:shared-id");
        assert_eq!(h_token1, "k1:shared-id");

        // API 模式同样
        let body2 = json!({ "system": "shared-prompt" });
        let a = generate_session_hash(ua, &body2, ClientType::API, Some(1));
        let b = generate_session_hash(ua, &body2, ClientType::API, Some(2));
        assert_ne!(a, b, "API: 不同 token_id 必须独立 sticky");
    }
}
