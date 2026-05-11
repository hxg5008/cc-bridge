//! 简单的 Prometheus 文本格式 metrics 暴露。
//!
//! 不引入 `prometheus` / `metrics` crate 依赖, 用 `AtomicU64` 自己维护计数。
//! 维度有限 (platform / outcome) 的话手工列举即可, 维护成本远低于打 crate。
//!
//! 暴露端点: `GET /metrics`, 文本格式遵循 Prometheus exposition format。

use dashmap::DashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// 单账号的缓存统计 atomic counter（per-account 维度，重启清零）。
#[derive(Default)]
pub struct PerAccountCacheCounters {
    pub input_tokens: AtomicU64,
    pub cache_read_tokens: AtomicU64,
    pub cache_creation_5m_tokens: AtomicU64,
    pub cache_creation_1h_tokens: AtomicU64,
    pub sniffed_requests: AtomicU64,
}

/// 一次性快照（用于 API 输出）。
#[derive(serde::Serialize, Clone, Copy, Default)]
pub struct PerAccountCacheSnapshot {
    pub account_id: i64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_5m_tokens: u64,
    pub cache_creation_1h_tokens: u64,
    pub sniffed_requests: u64,
    pub total_tokens: u64,
    pub hit_rate_pct: f64,
    pub one_hour_share_pct: f64,
}

impl PerAccountCacheCounters {
    fn snapshot(&self, account_id: i64) -> PerAccountCacheSnapshot {
        let input = self.input_tokens.load(Ordering::Relaxed);
        let read = self.cache_read_tokens.load(Ordering::Relaxed);
        let c5m = self.cache_creation_5m_tokens.load(Ordering::Relaxed);
        let c1h = self.cache_creation_1h_tokens.load(Ordering::Relaxed);
        let sniffed = self.sniffed_requests.load(Ordering::Relaxed);
        let total = input + read + c5m + c1h;
        PerAccountCacheSnapshot {
            account_id,
            input_tokens: input,
            cache_read_tokens: read,
            cache_creation_5m_tokens: c5m,
            cache_creation_1h_tokens: c1h,
            sniffed_requests: sniffed,
            total_tokens: total,
            hit_rate_pct: if total > 0 { read as f64 / total as f64 * 100.0 } else { 0.0 },
            one_hour_share_pct: if c5m + c1h > 0 { c1h as f64 / (c5m + c1h) as f64 * 100.0 } else { 0.0 },
        }
    }
}

/// 单计数器 + 一组带标签的计数器.
pub struct Counters {
    // 网关请求总数 (按平台分桶)
    pub gateway_requests_claude: AtomicU64,
    pub gateway_requests_openai: AtomicU64,

    // 网关错误响应 (按平台分桶, 仅 5xx + 429)
    pub gateway_errors_claude_429: AtomicU64,
    pub gateway_errors_claude_5xx: AtomicU64,
    pub gateway_errors_openai_429: AtomicU64,
    pub gateway_errors_openai_5xx: AtomicU64,

    // OAuth refresh 计数
    pub oauth_refresh_claude_success: AtomicU64,
    pub oauth_refresh_claude_failure: AtomicU64,
    pub oauth_refresh_openai_success: AtomicU64,
    pub oauth_refresh_openai_failure: AtomicU64,

    // OpenAI 上游 5xx 触发账号切换的次数
    pub openai_failover_total: AtomicU64,

    // 账号选号被用量门禁过滤的次数
    pub openai_account_filtered_by_limit: AtomicU64,

    // ---- Anthropic /v1/messages 缓存命中监控（从响应 message_start 事件吸取）----
    // 累计新输入 token（不含 cache_read / cache_creation）
    pub anthropic_input_tokens_total: AtomicU64,
    // 累计命中缓存的 token（这是缓存红利的核心指标）
    pub anthropic_cache_read_tokens_total: AtomicU64,
    // 累计写入 5m 缓存的 token
    pub anthropic_cache_creation_5m_tokens_total: AtomicU64,
    // 累计写入 1h 缓存的 token（开了 enable_cache_ttl_1h_injection 才会非 0）
    pub anthropic_cache_creation_1h_tokens_total: AtomicU64,
    // 累计成功嗅探到 message_start usage 的请求数
    pub anthropic_usage_sniffed_total: AtomicU64,

    // ---- Sticky session fail-over 监控 ----
    // sticky 首选账号被临时限流但 sticky 保留（fail-over 软切到候补）的次数
    pub sticky_preserved_total: AtomicU64,
    // sticky 首选账号永久失效（账号 disabled / 被删 / 被 token 黑名单）导致 sticky 被删的次数
    pub sticky_evicted_total: AtomicU64,

    // ---- OAuth session_key 自愈监控 ----
    // refresh_token 失效后，用 session_key 重新走 cookie_auth 拿到新 token 的成功次数
    pub oauth_recovery_session_key_success: AtomicU64,
    // session_key 也失效（用户可能改了密码 / 主动登出），自愈失败的次数
    pub oauth_recovery_session_key_failure: AtomicU64,

    // ---- per-account 缓存命中率（用于按账号拆分统计）----
    // DashMap 保证并发写零锁；重启清零
    pub per_account_cache: DashMap<i64, PerAccountCacheCounters>,

    // ---- 后台任务失败计数 (修 Phase2: 让 prometheus 能告警异常累积) ----
    /// limit_store::flush_to_db 失败次数 (DB 写入异常)
    pub bg_flush_db_failure: AtomicU64,
    /// 启动后第一次请求触发的"老 sticky 命中已失效账号"次数
    /// (重启后 Redis sticky 还在但内存 LimitStore 是空的, 第一波请求容易撞已限流号)
    pub post_restart_first_select: AtomicU64,
    /// 同一账号短期内 (滑动 1h 内) 触发 session_key fallback 的次数累计
    /// (>3 次说明 session_key 也快挂了, 该告警人工补)
    pub oauth_fallback_per_account: DashMap<i64, AtomicU64>,
    /// 账号 disable 计数, 按 reason 分类 (修 E1):
    /// 让 prometheus 能告警 "短时间内多账号被 disable" 提前发现批量失效
    pub account_disabled_by_reason: DashMap<&'static str, AtomicU64>,
    /// Circuit breaker open / close 事件计数 (修 N6):
    /// open 增量 → 短时间多个号撞 5xx; close 增量 → 上游恢复
    pub circuit_breaker_events: DashMap<&'static str, AtomicU64>,
    /// 拒绝事件计数 (修 N7): reason ∈ {global_cap, per_token_cap}
    /// 商业告警: per_token_cap 持续 > 0 说明有客户端在打 burst
    pub gateway_rejected_by_reason: DashMap<&'static str, AtomicU64>,

    /// 软恢复 cooldown 触发计数 — 收到 403 但未触阈, 设了 cooldown 跳过本号
    pub anthropic_403_cooldown_set: AtomicU64,
    /// 软恢复成功计数 — cooldown 后真实请求拿到 2xx, 清空 strikes
    pub anthropic_403_recovery: AtomicU64,

    // 启动时间 (gauge, 一次性写入, 用于计算 uptime)
    pub started_at_unix: AtomicI64,
}

impl Counters {
    pub fn new() -> Self {
        Self {
            gateway_requests_claude: AtomicU64::new(0),
            gateway_requests_openai: AtomicU64::new(0),
            gateway_errors_claude_429: AtomicU64::new(0),
            gateway_errors_claude_5xx: AtomicU64::new(0),
            gateway_errors_openai_429: AtomicU64::new(0),
            gateway_errors_openai_5xx: AtomicU64::new(0),
            oauth_refresh_claude_success: AtomicU64::new(0),
            oauth_refresh_claude_failure: AtomicU64::new(0),
            oauth_refresh_openai_success: AtomicU64::new(0),
            oauth_refresh_openai_failure: AtomicU64::new(0),
            openai_failover_total: AtomicU64::new(0),
            openai_account_filtered_by_limit: AtomicU64::new(0),
            anthropic_input_tokens_total: AtomicU64::new(0),
            anthropic_cache_read_tokens_total: AtomicU64::new(0),
            anthropic_cache_creation_5m_tokens_total: AtomicU64::new(0),
            anthropic_cache_creation_1h_tokens_total: AtomicU64::new(0),
            anthropic_usage_sniffed_total: AtomicU64::new(0),
            sticky_preserved_total: AtomicU64::new(0),
            sticky_evicted_total: AtomicU64::new(0),
            oauth_recovery_session_key_success: AtomicU64::new(0),
            oauth_recovery_session_key_failure: AtomicU64::new(0),
            per_account_cache: DashMap::new(),
            bg_flush_db_failure: AtomicU64::new(0),
            post_restart_first_select: AtomicU64::new(0),
            oauth_fallback_per_account: DashMap::new(),
            account_disabled_by_reason: DashMap::new(),
            circuit_breaker_events: DashMap::new(),
            gateway_rejected_by_reason: DashMap::new(),
            anthropic_403_cooldown_set: AtomicU64::new(0),
            anthropic_403_recovery: AtomicU64::new(0),
            started_at_unix: AtomicI64::new(0),
        }
    }

    /// limit_store::flush_to_db 失败计数 (Phase2: 让 prometheus 能告警异常累积)
    pub fn record_bg_flush_failure(&self) {
        self.bg_flush_db_failure.fetch_add(1, Ordering::Relaxed);
    }

    /// 账号被 disable_account 时上报 (修 E1):
    /// 让运维能配 `increase(...{reason="auth_403"}[5m]) > 5` 告警
    /// 检测"短时间内多个号挂掉"。
    pub fn record_account_disabled(&self, reason_label: &'static str) {
        let entry = self
            .account_disabled_by_reason
            .entry(reason_label)
            .or_insert_with(|| AtomicU64::new(0));
        entry.fetch_add(1, Ordering::Relaxed);
    }

    /// 软恢复 — 上游 403 但未达永久 disable 阈值时调用 (设 cooldown 跳过本号)
    pub fn record_403_cooldown_set(&self) {
        self.anthropic_403_cooldown_set.fetch_add(1, Ordering::Relaxed);
    }

    /// 软恢复 — cooldown 后真实请求拿到 2xx, 清空 strikes 时调用
    pub fn record_403_recovery(&self) {
        self.anthropic_403_recovery.fetch_add(1, Ordering::Relaxed);
    }

    /// Circuit breaker 状态变化 (修 N6): event ∈ {"open", "close"}
    pub fn record_circuit_event(&self, event: &'static str) {
        let entry = self
            .circuit_breaker_events
            .entry(event)
            .or_insert_with(|| AtomicU64::new(0));
        entry.fetch_add(1, Ordering::Relaxed);
    }

    /// 网关拒绝请求 (修 N7): reason ∈ {"global_cap", "per_token_cap"}
    pub fn record_gateway_rejected(&self, reason: &'static str) {
        let entry = self
            .gateway_rejected_by_reason
            .entry(reason)
            .or_insert_with(|| AtomicU64::new(0));
        entry.fetch_add(1, Ordering::Relaxed);
    }

    /// 记录某账号的 session_key fallback 累计次数
    /// (>3 次的账号说明 session_key 也快挂, 应人工告警)
    pub fn record_oauth_fallback_for_account(&self, account_id: i64) -> u64 {
        let entry = self
            .oauth_fallback_per_account
            .entry(account_id)
            .or_insert_with(|| AtomicU64::new(0));
        entry.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// 重启后第一波请求选号时, sticky 命中已限流账号的次数
    pub fn record_post_restart_first_select(&self) {
        self.post_restart_first_select.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_gateway_request(&self, platform: Platform) {
        match platform {
            Platform::Claude => self.gateway_requests_claude.fetch_add(1, Ordering::Relaxed),
            Platform::OpenAI => self.gateway_requests_openai.fetch_add(1, Ordering::Relaxed),
        };
    }

    pub fn record_gateway_error(&self, platform: Platform, status: u16) {
        match (platform, status) {
            (Platform::Claude, 429) => self.gateway_errors_claude_429.fetch_add(1, Ordering::Relaxed),
            (Platform::Claude, s) if (500..600).contains(&s) => {
                self.gateway_errors_claude_5xx.fetch_add(1, Ordering::Relaxed)
            }
            (Platform::OpenAI, 429) => self.gateway_errors_openai_429.fetch_add(1, Ordering::Relaxed),
            (Platform::OpenAI, s) if (500..600).contains(&s) => {
                self.gateway_errors_openai_5xx.fetch_add(1, Ordering::Relaxed)
            }
            _ => 0,
        };
    }

    pub fn record_oauth_refresh(&self, platform: Platform, ok: bool) {
        match (platform, ok) {
            (Platform::Claude, true) => self.oauth_refresh_claude_success.fetch_add(1, Ordering::Relaxed),
            (Platform::Claude, false) => self.oauth_refresh_claude_failure.fetch_add(1, Ordering::Relaxed),
            (Platform::OpenAI, true) => self.oauth_refresh_openai_success.fetch_add(1, Ordering::Relaxed),
            (Platform::OpenAI, false) => self.oauth_refresh_openai_failure.fetch_add(1, Ordering::Relaxed),
        };
    }

    pub fn record_failover(&self) {
        self.openai_failover_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_filtered_by_limit(&self, n: u64) {
        if n > 0 {
            self.openai_account_filtered_by_limit
                .fetch_add(n, Ordering::Relaxed);
        }
    }

    /// 累加 Anthropic /v1/messages 响应中嗅探到的 usage 字段。
    /// 由 UsageSnifferStream 在解析到第一个 message_start 事件时调用。
    /// 同时累加到全局 counter 和 per-account counter。
    pub fn record_anthropic_usage(&self, account_id: i64, usage: &AnthropicUsageSnapshot) {
        // 1. 全局累加（保持向后兼容）
        self.anthropic_input_tokens_total
            .fetch_add(usage.input_tokens, Ordering::Relaxed);
        self.anthropic_cache_read_tokens_total
            .fetch_add(usage.cache_read_input_tokens, Ordering::Relaxed);
        self.anthropic_cache_creation_5m_tokens_total
            .fetch_add(usage.ephemeral_5m_input_tokens, Ordering::Relaxed);
        self.anthropic_cache_creation_1h_tokens_total
            .fetch_add(usage.ephemeral_1h_input_tokens, Ordering::Relaxed);
        self.anthropic_usage_sniffed_total
            .fetch_add(1, Ordering::Relaxed);

        // 2. per-account 累加（按 account_id 分桶）
        if account_id > 0 {
            let entry = self
                .per_account_cache
                .entry(account_id)
                .or_default();
            entry.input_tokens.fetch_add(usage.input_tokens, Ordering::Relaxed);
            entry
                .cache_read_tokens
                .fetch_add(usage.cache_read_input_tokens, Ordering::Relaxed);
            entry
                .cache_creation_5m_tokens
                .fetch_add(usage.ephemeral_5m_input_tokens, Ordering::Relaxed);
            entry
                .cache_creation_1h_tokens
                .fetch_add(usage.ephemeral_1h_input_tokens, Ordering::Relaxed);
            entry.sniffed_requests.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// 取所有账号的 cache 统计快照（用于 admin API 输出）。
    pub fn snapshot_per_account_cache(&self) -> Vec<PerAccountCacheSnapshot> {
        self.per_account_cache
            .iter()
            .map(|kv| kv.value().snapshot(*kv.key()))
            .collect()
    }

    /// 删账号时调用: 清理该账号在 metrics 里的 per-account counters,
    /// 防止删号后内存里"幽灵账号"持续累积。
    pub fn remove_account(&self, account_id: i64) {
        self.per_account_cache.remove(&account_id);
    }

    /// sticky 首选账号被临时限流时调用：保留 sticky，本次走 fallback。
    pub fn record_sticky_preserved(&self) {
        self.sticky_preserved_total.fetch_add(1, Ordering::Relaxed);
    }

    /// sticky 首选账号永久失效时调用：删 sticky，下次重新随机绑。
    pub fn record_sticky_evicted(&self) {
        self.sticky_evicted_total.fetch_add(1, Ordering::Relaxed);
    }

    /// refresh_token 失效后，用 session_key 重新走 cookie_auth 是否成功。
    pub fn record_oauth_recovery_session_key(&self, ok: bool) {
        if ok {
            self.oauth_recovery_session_key_success
                .fetch_add(1, Ordering::Relaxed);
        } else {
            self.oauth_recovery_session_key_failure
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn mark_started(&self) {
        self.started_at_unix
            .store(chrono::Utc::now().timestamp(), Ordering::Relaxed);
    }

    /// 渲染为 Prometheus 文本格式. 返回 String 以便直接作为 HTTP body。
    pub fn render(&self, accounts_by_status: &AccountsByStatus) -> String {
        let mut out = String::with_capacity(2048);
        let g = |out: &mut String, name: &str, help: &str, typ: &str, labels: &str, val: u64| {
            if !labels.is_empty() {
                let _ = std::fmt::Write::write_fmt(
                    out,
                    format_args!("# HELP {} {}\n# TYPE {} {}\n{}{{{}}} {}\n", name, help, name, typ, name, labels, val),
                );
            } else {
                let _ = std::fmt::Write::write_fmt(
                    out,
                    format_args!("# HELP {} {}\n# TYPE {} {}\n{} {}\n", name, help, name, typ, name, val),
                );
            }
        };

        g(
            &mut out,
            "ccbridge_gateway_requests_total",
            "Total gateway requests by platform",
            "counter",
            "platform=\"claude\"",
            self.gateway_requests_claude.load(Ordering::Relaxed),
        );
        g(
            &mut out,
            "ccbridge_gateway_requests_total",
            "",
            "counter",
            "platform=\"openai\"",
            self.gateway_requests_openai.load(Ordering::Relaxed),
        );

        for (platform, status, val) in [
            ("claude", "429", self.gateway_errors_claude_429.load(Ordering::Relaxed)),
            ("claude", "5xx", self.gateway_errors_claude_5xx.load(Ordering::Relaxed)),
            ("openai", "429", self.gateway_errors_openai_429.load(Ordering::Relaxed)),
            ("openai", "5xx", self.gateway_errors_openai_5xx.load(Ordering::Relaxed)),
        ] {
            g(
                &mut out,
                "ccbridge_gateway_errors_total",
                "Gateway error responses by platform and status class",
                "counter",
                &format!("platform=\"{}\",status=\"{}\"", platform, status),
                val,
            );
        }

        for (platform, outcome, val) in [
            ("claude", "success", self.oauth_refresh_claude_success.load(Ordering::Relaxed)),
            ("claude", "failure", self.oauth_refresh_claude_failure.load(Ordering::Relaxed)),
            ("openai", "success", self.oauth_refresh_openai_success.load(Ordering::Relaxed)),
            ("openai", "failure", self.oauth_refresh_openai_failure.load(Ordering::Relaxed)),
        ] {
            g(
                &mut out,
                "ccbridge_oauth_refresh_total",
                "OAuth token refresh outcomes by platform",
                "counter",
                &format!("platform=\"{}\",outcome=\"{}\"", platform, outcome),
                val,
            );
        }

        g(
            &mut out,
            "ccbridge_openai_failover_total",
            "OpenAI 5xx-triggered account failovers",
            "counter",
            "",
            self.openai_failover_total.load(Ordering::Relaxed),
        );
        g(
            &mut out,
            "ccbridge_openai_account_filtered_by_limit_total",
            "OpenAI accounts filtered by codex usage / 429 cooldown during scheduling",
            "counter",
            "",
            self.openai_account_filtered_by_limit.load(Ordering::Relaxed),
        );

        // Anthropic /v1/messages 缓存命中指标 ----
        // 命中率计算: cache_read / (input + cache_read + cache_creation_5m + cache_creation_1h)
        for (kind, val) in [
            ("input", self.anthropic_input_tokens_total.load(Ordering::Relaxed)),
            ("cache_read", self.anthropic_cache_read_tokens_total.load(Ordering::Relaxed)),
            ("cache_creation_5m", self.anthropic_cache_creation_5m_tokens_total.load(Ordering::Relaxed)),
            ("cache_creation_1h", self.anthropic_cache_creation_1h_tokens_total.load(Ordering::Relaxed)),
        ] {
            g(
                &mut out,
                "ccbridge_anthropic_tokens_total",
                "Anthropic /v1/messages tokens by category (sniffed from message_start usage)",
                "counter",
                &format!("kind=\"{}\"", kind),
                val,
            );
        }
        g(
            &mut out,
            "ccbridge_anthropic_usage_sniffed_total",
            "Number of /v1/messages responses where usage was successfully sniffed",
            "counter",
            "",
            self.anthropic_usage_sniffed_total.load(Ordering::Relaxed),
        );

        // Sticky session fail-over 指标
        g(
            &mut out,
            "ccbridge_sticky_preserved_total",
            "Sticky session preserved despite primary account temporarily rate-limited (soft fail-over)",
            "counter",
            "",
            self.sticky_preserved_total.load(Ordering::Relaxed),
        );
        g(
            &mut out,
            "ccbridge_sticky_evicted_total",
            "Sticky session evicted because primary account permanently unavailable (disabled / blocked)",
            "counter",
            "",
            self.sticky_evicted_total.load(Ordering::Relaxed),
        );

        // OAuth session_key 自愈指标
        for (outcome, val) in [
            ("success", self.oauth_recovery_session_key_success.load(Ordering::Relaxed)),
            ("failure", self.oauth_recovery_session_key_failure.load(Ordering::Relaxed)),
        ] {
            g(
                &mut out,
                "ccbridge_oauth_recovery_session_key_total",
                "OAuth recovery via stored session_key (when refresh_token failed)",
                "counter",
                &format!("outcome=\"{}\"", outcome),
                val,
            );
        }

        // 后台任务失败 (告警关键指标: 持续 >0 说明 DB 异常)
        g(
            &mut out,
            "ccbridge_bg_flush_db_failure_total",
            "limit_store flush_to_db failures (background tasks)",
            "counter",
            "",
            self.bg_flush_db_failure.load(Ordering::Relaxed),
        );

        // 重启后第一波 sticky 命中已限流账号的次数
        g(
            &mut out,
            "ccbridge_post_restart_first_select_total",
            "Sticky hits routing to already-rate-limited account after restart",
            "counter",
            "",
            self.post_restart_first_select.load(Ordering::Relaxed),
        );

        // 每账号 OAuth fallback 累计 (>3 的账号需要人工介入)
        for kv in self.oauth_fallback_per_account.iter() {
            g(
                &mut out,
                "ccbridge_oauth_fallback_per_account_total",
                "Per-account session_key fallback invocations (>3 = session_key likely stale)",
                "counter",
                &format!("account=\"{}\"", kv.key()),
                kv.value().load(Ordering::Relaxed),
            );
        }

        // 账号 disable 按 reason 分桶 (告警关键: 短时间多账号 disable = 批量失效)
        for kv in self.account_disabled_by_reason.iter() {
            g(
                &mut out,
                "ccbridge_account_disabled_total",
                "Total accounts disabled, bucketed by reason classification",
                "counter",
                &format!("reason=\"{}\"", kv.key()),
                kv.value().load(Ordering::Relaxed),
            );
        }

        // Circuit breaker 事件 (修 N6): open / close 计数, 上线后 open 增量异常 → 上游故障
        for kv in self.circuit_breaker_events.iter() {
            g(
                &mut out,
                "ccbridge_circuit_breaker_events_total",
                "Per-account circuit breaker state transitions (open/close)",
                "counter",
                &format!("event=\"{}\"", kv.key()),
                kv.value().load(Ordering::Relaxed),
            );
        }

        // 网关拒绝事件 (修 N7): 来自 global cap / per-token cap
        // per_token_cap 持续 > 0 → 有客户端在打 burst, 适合做客户端限频沟通
        for kv in self.gateway_rejected_by_reason.iter() {
            g(
                &mut out,
                "ccbridge_gateway_rejected_total",
                "Total gateway requests rejected before reaching upstream",
                "counter",
                &format!("reason=\"{}\"", kv.key()),
                kv.value().load(Ordering::Relaxed),
            );
        }

        // gauges (账号实时状态)
        for (platform, status, val) in [
            ("claude", "active", accounts_by_status.claude_active),
            ("claude", "error", accounts_by_status.claude_error),
            ("claude", "disabled", accounts_by_status.claude_disabled),
            ("openai", "active", accounts_by_status.openai_active),
            ("openai", "error", accounts_by_status.openai_error),
            ("openai", "disabled", accounts_by_status.openai_disabled),
        ] {
            g(
                &mut out,
                "ccbridge_accounts",
                "Account count by platform and status",
                "gauge",
                &format!("platform=\"{}\",status=\"{}\"", platform, status),
                val,
            );
        }

        let started = self.started_at_unix.load(Ordering::Relaxed);
        let uptime = if started > 0 {
            (chrono::Utc::now().timestamp() - started).max(0) as u64
        } else {
            0
        };
        g(
            &mut out,
            "ccbridge_uptime_seconds",
            "Seconds since gateway started",
            "gauge",
            "",
            uptime,
        );

        out
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Platform {
    Claude,
    OpenAI,
}

/// Anthropic /v1/messages 响应里的 usage 快照。
/// 由 UsageSnifferStream 从 message_start SSE 事件解析出来，喂给 record_anthropic_usage。
#[derive(Debug, Default, Clone, Copy)]
pub struct AnthropicUsageSnapshot {
    pub input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub ephemeral_5m_input_tokens: u64,
    pub ephemeral_1h_input_tokens: u64,
}

#[derive(Debug, Default)]
pub struct AccountsByStatus {
    pub claude_active: u64,
    pub claude_error: u64,
    pub claude_disabled: u64,
    pub openai_active: u64,
    pub openai_error: u64,
    pub openai_disabled: u64,
}

/// 全局单例 — main.rs 启动时调 mark_started, 各 hot path 调 record_xxx。
pub static METRICS: once_cell::sync::Lazy<Counters> =
    once_cell::sync::Lazy::new(Counters::new);
