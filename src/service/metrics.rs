//! 简单的 Prometheus 文本格式 metrics 暴露。
//!
//! 不引入 `prometheus` / `metrics` crate 依赖, 用 `AtomicU64` 自己维护计数。
//! 维度有限 (platform / outcome) 的话手工列举即可, 维护成本远低于打 crate。
//!
//! 暴露端点: `GET /metrics`, 文本格式遵循 Prometheus exposition format。

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

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

    // 启动时间 (gauge, 一次性写入, 用于计算 uptime)
    pub started_at_unix: AtomicI64,
}

impl Counters {
    pub const fn new() -> Self {
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
            started_at_unix: AtomicI64::new(0),
        }
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
pub static METRICS: Counters = Counters::new();
