//! ChatGPT 网页隐私设置 + 账号信息回填。
//!
//! 拿到 OpenAI OAuth access_token 之后两件事:
//!
//! 1. **关训练数据共享** — `PATCH /backend-api/settings/account_user_setting?feature=training_allowed&value=false`
//!    这是 sub2api 一直在跑的标准防风控步骤,默认 ChatGPT 网页"训练数据共享"开关是 ON。
//!    Best-effort,失败不阻断。
//!
//! 2. **拿真实 plan_type / email / subscription** — `GET /backend-api/accounts/check/v4-2023-04-27`
//!    id_token 不一定带这些信息(尤其 mobile RT),从这个端点能拿到准确的订阅信息。
//!
//! 移植自 sub2api `openai_privacy_service.go`。

use std::time::Duration;

use serde_json::Value;
use tracing::{debug, info, warn};

const SETTINGS_URL: &str = "https://chatgpt.com/backend-api/settings/account_user_setting";
const ACCOUNTS_CHECK_URL: &str = "https://chatgpt.com/backend-api/accounts/check/v4-2023-04-27";
const ENRICH_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivacyMode {
    /// 调用未发起 (没 token / factory 不可用)。
    Skipped,
    /// 训练共享已关闭。
    TrainingOff,
    /// 调用失败 (网络 / 上游 5xx 等)。
    Failed,
    /// 上游返回 Cloudflare 拦截 (403/503 + 含 cf 标记)。
    CFBlocked,
}

impl PrivacyMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Skipped => "",
            Self::TrainingOff => "training_off",
            Self::Failed => "training_set_failed",
            Self::CFBlocked => "training_set_cf_blocked",
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct ChatGPTAccountInfo {
    pub plan_type: String,
    pub email: String,
    pub subscription_expires_at: String,
}

/// PATCH chatgpt.com 把 `training_allowed` 设成 false。
pub async fn disable_openai_training(access_token: &str, proxy_url: &str) -> PrivacyMode {
    if access_token.trim().is_empty() {
        return PrivacyMode::Skipped;
    }

    let client = crate::tlsfp::make_request_client(proxy_url);
    let req = client
        .patch(SETTINGS_URL)
        .timeout(ENRICH_TIMEOUT)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Origin", "https://chatgpt.com")
        .header("Referer", "https://chatgpt.com/")
        .header("Accept", "application/json")
        .header("sec-fetch-mode", "cors")
        .header("sec-fetch-site", "same-origin")
        .header("sec-fetch-dest", "empty")
        .query(&[("feature", "training_allowed"), ("value", "false")]);

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            warn!(target: "openai_privacy", "request_error: {}", e);
            return PrivacyMode::Failed;
        }
    };

    let status = resp.status();
    if status == 403 || status == 503 {
        let body = resp.text().await.unwrap_or_default();
        if body.contains("cloudflare") || body.contains("cf-") || body.contains("Just a moment") {
            warn!(target: "openai_privacy", "cf_blocked status={}", status);
            return PrivacyMode::CFBlocked;
        }
        warn!(target: "openai_privacy", "failed status={} body_len={}", status, body.len());
        return PrivacyMode::Failed;
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        warn!(
            target: "openai_privacy",
            "failed status={} body={}",
            status,
            truncate(&body, 200)
        );
        return PrivacyMode::Failed;
    }
    info!(target: "openai_privacy", "training_disabled");
    PrivacyMode::TrainingOff
}

/// GET `/backend-api/accounts/check/v4-2023-04-27` 拿真实 plan_type / email / subscription.
///
/// `org_id` 来自 access_token JWT 的 `poid` 或 id_token 的 `organization_id`,
/// 用于在多账号场景挑出正确的那个;空时按"默认 → 非 free → 任意"优先级回退。
pub async fn fetch_chatgpt_account_info(
    access_token: &str,
    proxy_url: &str,
    org_id: &str,
) -> Option<ChatGPTAccountInfo> {
    if access_token.trim().is_empty() {
        return None;
    }

    let client = crate::tlsfp::make_request_client(proxy_url);
    let resp = match client
        .get(ACCOUNTS_CHECK_URL)
        .timeout(ENRICH_TIMEOUT)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Origin", "https://chatgpt.com")
        .header("Referer", "https://chatgpt.com/")
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            debug!(target: "openai_privacy", "accounts_check_request_error: {}", e);
            return None;
        }
    };

    if !resp.status().is_success() {
        debug!(target: "openai_privacy", "accounts_check_status={}", resp.status());
        return None;
    }
    let json: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            debug!(target: "openai_privacy", "accounts_check_decode_error: {}", e);
            return None;
        }
    };

    let accounts = json.get("accounts").and_then(|v| v.as_object())?;

    let mut info = ChatGPTAccountInfo::default();

    // 1) 优先 org_id 匹配
    if !org_id.is_empty() {
        if let Some(acct) = accounts.get(org_id) {
            fill_account_info(&mut info, acct);
        }
    }

    // 2) 否则按优先级 default → 非 free → 任意
    if info.plan_type.is_empty() {
        let mut default_c = ChatGPTAccountInfo::default();
        let mut paid_c = ChatGPTAccountInfo::default();
        let mut any_c = ChatGPTAccountInfo::default();
        for acct in accounts.values() {
            let plan_type = extract_plan_type(acct);
            if plan_type.is_empty() {
                continue;
            }
            let expires = extract_entitlement_expires_at(acct);
            let email = extract_email(acct);
            if any_c.plan_type.is_empty() {
                any_c = ChatGPTAccountInfo {
                    plan_type: plan_type.clone(),
                    email: email.clone(),
                    subscription_expires_at: expires.clone(),
                };
            }
            if let Some(account) = acct.get("account").and_then(|v| v.as_object()) {
                if account.get("is_default").and_then(|v| v.as_bool()).unwrap_or(false) {
                    default_c = ChatGPTAccountInfo {
                        plan_type: plan_type.clone(),
                        email: email.clone(),
                        subscription_expires_at: expires.clone(),
                    };
                }
            }
            if !plan_type.eq_ignore_ascii_case("free") && paid_c.plan_type.is_empty() {
                paid_c = ChatGPTAccountInfo {
                    plan_type,
                    email,
                    subscription_expires_at: expires,
                };
            }
        }
        info = if !default_c.plan_type.is_empty() {
            default_c
        } else if !paid_c.plan_type.is_empty() {
            paid_c
        } else {
            any_c
        };
    }

    if info.plan_type.is_empty() {
        debug!(target: "openai_privacy", "accounts_check_no_plan_type");
        return None;
    }

    info!(
        target: "openai_privacy",
        "accounts_check_success plan_type={} subscription_expires_at={} org_id={}",
        info.plan_type,
        info.subscription_expires_at,
        org_id
    );
    Some(info)
}

fn fill_account_info(info: &mut ChatGPTAccountInfo, acct: &Value) {
    info.plan_type = extract_plan_type(acct);
    info.subscription_expires_at = extract_entitlement_expires_at(acct);
    if info.email.is_empty() {
        info.email = extract_email(acct);
    }
}

fn extract_plan_type(acct: &Value) -> String {
    if let Some(account) = acct.get("account").and_then(|v| v.as_object()) {
        if let Some(s) = account.get("plan_type").and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    if let Some(entitlement) = acct.get("entitlement").and_then(|v| v.as_object()) {
        if let Some(s) = entitlement.get("subscription_plan").and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    String::new()
}

fn extract_entitlement_expires_at(acct: &Value) -> String {
    acct.get("entitlement")
        .and_then(|v| v.as_object())
        .and_then(|m| m.get("expires_at"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn extract_email(acct: &Value) -> String {
    if let Some(user) = acct.get("user").and_then(|v| v.as_object()) {
        if let Some(s) = user.get("email").and_then(|v| v.as_str()) {
            return s.to_string();
        }
    }
    if let Some(s) = acct.get("email").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    String::new()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...({} more)", &s[..max], s.len() - max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_plan_type_from_account_field() {
        let v = json!({"account": {"plan_type": "plus"}});
        assert_eq!(extract_plan_type(&v), "plus");
    }

    #[test]
    fn extract_plan_type_from_entitlement_fallback() {
        let v = json!({"entitlement": {"subscription_plan": "team"}});
        assert_eq!(extract_plan_type(&v), "team");
    }

    #[test]
    fn extract_plan_type_empty_when_absent() {
        let v = json!({});
        assert_eq!(extract_plan_type(&v), "");
    }

    #[test]
    fn privacy_mode_strings() {
        assert_eq!(PrivacyMode::TrainingOff.as_str(), "training_off");
        assert_eq!(PrivacyMode::Failed.as_str(), "training_set_failed");
        assert_eq!(PrivacyMode::CFBlocked.as_str(), "training_set_cf_blocked");
        assert_eq!(PrivacyMode::Skipped.as_str(), "");
    }
}
