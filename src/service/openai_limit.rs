//! OpenAI 账号可调度性判定 + 429 冷却写入。
//!
//! 判定数据源是 `Account.extra` (异步落库 by `parse_codex_rate_limit_headers` /
//! `persist_codex_usage` in router.rs),包含的字段:
//!
//! - `codex_usage_5h_used_percent` (f64) / `codex_usage_5h_reset_at` (i64 unix sec)
//! - `codex_usage_7d_used_percent` (f64) / `codex_usage_7d_reset_at` (i64 unix sec)
//! - `codex_429_until` (i64 unix sec) — 短期 429 冷却,本模块写入
//!
//! ChatGPT 内部 Codex 端点没有像 Anthropic 那样暴露 representative_claim,
//! 所以这里没法做模型粒度的回避(Opus/Sonnet 区分),只能整账号级。
//! 上游真发了 429,我们就让账号冷却 60 秒(或 Retry-After 指定的时间)再回池。

use serde_json::Value;

use crate::model::account::Account;

/// 5h / 7d 窗口已用百分比超过这个值视为账号"满"。
///
/// 与 sub2api Claude `LimitStore` 5h/7d 阈值保持一致 (97%) — Codex CLI 自己显示到
/// 100% 才提示,留 3% 头空间避免大查询撑爆。
pub const CODEX_HARD_CAP_PERCENT: f64 = 97.0;

/// 上游没给 `Retry-After` 头时,默认冷却 60 秒。
pub const DEFAULT_429_COOLDOWN_SECONDS: i64 = 60;

/// 判断 OpenAI 账号是否当前可调度。
///
/// 任一条件命中 → 不可调度:
/// 1. `codex_429_until > now` (短期 429 冷却中)
/// 2. `codex_usage_5h_used_percent >= 97%` 且 `codex_usage_5h_reset_at > now` (5h 满)
/// 3. `codex_usage_7d_used_percent >= 97%` 且 `codex_usage_7d_reset_at > now` (7d 满)
pub fn openai_schedulable(account: &Account) -> bool {
    let now = chrono::Utc::now().timestamp();
    let extra = match account.extra.as_object() {
        Some(o) => o,
        None => return true,
    };

    if let Some(until) = read_i64(extra.get("codex_429_until")) {
        if until > now {
            return false;
        }
    }

    if window_blocked(extra, "codex_usage_5h_used_percent", "codex_usage_5h_reset_at", now) {
        return false;
    }
    if window_blocked(extra, "codex_usage_7d_used_percent", "codex_usage_7d_reset_at", now) {
        return false;
    }

    true
}

/// 计算上游 429 应触发的 cooldown 截止时间戳 (unix seconds)。
///
/// 优先解析 `Retry-After` 头;支持纯秒数,不支持 HTTP-date(Codex 端点不会返回这种)。
/// 缺省或解析失败时回退到 [`DEFAULT_429_COOLDOWN_SECONDS`]。
pub fn compute_429_cooldown_until(retry_after_header: Option<&str>) -> i64 {
    let now = chrono::Utc::now().timestamp();
    let secs = retry_after_header
        .and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|n| *n > 0 && *n <= 24 * 3600)
        .unwrap_or(DEFAULT_429_COOLDOWN_SECONDS);
    now + secs
}

/// 把 `codex_429_until` 字段 merge 到 extra,其它字段保留。
///
/// 调用方负责把返回的 Value 写回 account.extra 并 update_account。
pub fn merge_429_into_extra(existing: &Value, until_unix: i64) -> Value {
    let mut current = match existing.as_object() {
        Some(o) => o.clone(),
        None => serde_json::Map::new(),
    };
    current.insert(
        "codex_429_until".into(),
        Value::Number(serde_json::Number::from(until_unix)),
    );
    Value::Object(current)
}

// ---------------------------------------------------------------------------
// 内部辅助
// ---------------------------------------------------------------------------

fn window_blocked(
    extra: &serde_json::Map<String, Value>,
    pct_key: &str,
    reset_key: &str,
    now: i64,
) -> bool {
    let pct = match read_f64(extra.get(pct_key)) {
        Some(p) => p,
        None => return false,
    };
    if pct < CODEX_HARD_CAP_PERCENT {
        return false;
    }
    match read_i64(extra.get(reset_key)) {
        // 没有 reset 时间但已知到了 97% → 保险起见也算 blocked
        None => true,
        Some(r) => r > now,
    }
}

fn read_f64(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

fn read_i64(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::account::{
        Account, AccountAuthType, AccountStatus, BillingMode,
    };
    use chrono::Utc;
    use serde_json::json;

    fn account_with_extra(extra: Value) -> Account {
        Account {
            id: 1,
            name: "t".into(),
            email: "t@t".into(),
            status: AccountStatus::Active,
            auth_type: AccountAuthType::Oauth,
            setup_token: String::new(),
            access_token: "tok".into(),
            refresh_token: "ref".into(),
            expires_at: None,
            oauth_refreshed_at: None,
            auth_error: String::new(),
            proxy_url: String::new(),
            device_id: String::new(),
            canonical_env: json!({}),
            canonical_prompt: json!({}),
            canonical_process: json!({}),
            billing_mode: BillingMode::Strip,
            account_uuid: None,
            organization_uuid: None,
            subscription_type: None,
            concurrency: 3,
            priority: 50,
            rate_limited_at: None,
            rate_limit_reset_at: None,
            disable_reason: String::new(),
            auto_telemetry: false,
            telemetry_count: 0,
            usage_data: json!({}),
            usage_fetched_at: None,
            platform: "openai".into(),
            extra,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn empty_extra_is_schedulable() {
        let a = account_with_extra(json!({}));
        assert!(openai_schedulable(&a));
    }

    #[test]
    fn under_cap_is_schedulable() {
        let now = Utc::now().timestamp();
        let a = account_with_extra(json!({
            "codex_usage_5h_used_percent": 80.0,
            "codex_usage_5h_reset_at": now + 3600,
            "codex_usage_7d_used_percent": 50.0,
            "codex_usage_7d_reset_at": now + 7 * 86400,
        }));
        assert!(openai_schedulable(&a));
    }

    #[test]
    fn five_hour_full_blocks() {
        let now = Utc::now().timestamp();
        let a = account_with_extra(json!({
            "codex_usage_5h_used_percent": 99.0,
            "codex_usage_5h_reset_at": now + 600,
        }));
        assert!(!openai_schedulable(&a));
    }

    #[test]
    fn seven_day_full_blocks() {
        let now = Utc::now().timestamp();
        let a = account_with_extra(json!({
            "codex_usage_7d_used_percent": 97.5,
            "codex_usage_7d_reset_at": now + 86400,
        }));
        assert!(!openai_schedulable(&a));
    }

    #[test]
    fn already_reset_does_not_block() {
        let now = Utc::now().timestamp();
        let a = account_with_extra(json!({
            "codex_usage_5h_used_percent": 99.0,
            "codex_usage_5h_reset_at": now - 60,
        }));
        assert!(openai_schedulable(&a));
    }

    #[test]
    fn active_429_cooldown_blocks() {
        let now = Utc::now().timestamp();
        let a = account_with_extra(json!({"codex_429_until": now + 30}));
        assert!(!openai_schedulable(&a));
    }

    #[test]
    fn expired_429_cooldown_does_not_block() {
        let now = Utc::now().timestamp();
        let a = account_with_extra(json!({"codex_429_until": now - 30}));
        assert!(openai_schedulable(&a));
    }

    #[test]
    fn compute_cooldown_uses_retry_after() {
        let now = Utc::now().timestamp();
        let until = compute_429_cooldown_until(Some("120"));
        assert!(until >= now + 119 && until <= now + 121);
    }

    #[test]
    fn compute_cooldown_falls_back_to_default() {
        let now = Utc::now().timestamp();
        let until = compute_429_cooldown_until(None);
        let want = now + DEFAULT_429_COOLDOWN_SECONDS;
        assert!((until - want).abs() <= 1);
        // 异常值不被采纳
        let bad = compute_429_cooldown_until(Some("not-a-number"));
        assert!((bad - want).abs() <= 1);
        let zero = compute_429_cooldown_until(Some("0"));
        assert!((zero - want).abs() <= 1);
        let too_big = compute_429_cooldown_until(Some("99999999"));
        assert!((too_big - want).abs() <= 1);
    }

    #[test]
    fn merge_429_preserves_other_fields() {
        let existing = json!({
            "chatgpt_account_id": "abc",
            "codex_usage_5h_used_percent": 50.0,
        });
        let merged = merge_429_into_extra(&existing, 1234567890);
        assert_eq!(merged["chatgpt_account_id"], "abc");
        assert_eq!(merged["codex_usage_5h_used_percent"], 50.0);
        assert_eq!(merged["codex_429_until"], 1234567890);
    }

    #[test]
    fn missing_reset_with_full_percent_treated_as_blocked() {
        // 防御性: 数据残缺(没 reset_at)但已知到顶,不要冒险派单
        let a = account_with_extra(json!({"codex_usage_5h_used_percent": 99.0}));
        assert!(!openai_schedulable(&a));
    }
}
