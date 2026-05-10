use crate::error::AppError;
use crate::model::account::CanonicalEnvData;
use crate::tlsfp::make_request_client;
use chrono::{DateTime, Utc};
use serde::Deserialize;

/// 截断并屏蔽上游错误 body 中的敏感字段 (修 Y1):
/// 上游 OAuth/usage 出错时返回的 JSON 偶尔会回显 access_token / refresh_token /
/// email 等。如果整个 body 进 error 日志, PII 和凭证就跟着落盘。
/// 策略:
///   1. 长度截断到 200 字符 (足够定位错误类型, 不足以泄漏完整 token)
///   2. 屏蔽 Bearer XXX / sk-ant-XXX / sk-XXX 等模式
pub(crate) fn sanitize_upstream_error_body(text: &str) -> String {
    let trimmed: String = text.chars().take(200).collect();
    // 极简屏蔽: 不引入 regex 依赖, 用字符串扫描
    let mut out = String::with_capacity(trimmed.len());
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // 匹配 "Bearer " 后续到空格/末尾
        if bytes[i..].starts_with(b"Bearer ") {
            out.push_str("Bearer ***");
            i += 7;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'"' {
                i += 1;
            }
            continue;
        }
        // 匹配 sk-ant- / sk- 前缀的 token
        if bytes[i..].starts_with(b"sk-ant-") || bytes[i..].starts_with(b"sk-") {
            out.push_str("sk-***");
            // 跳过 token 主体 (字母数字 + - _)
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-' || bytes[i] == b'_')
            {
                i += 1;
            }
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}
use serde_json::Value;

const OAUTH_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const OAUTH_SCOPES: &[&str] = &[
    "user:profile",
    "user:inference",
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
];

#[derive(Debug, Clone)]
pub struct RefreshedOAuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct OAuthRefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_in: i64,
}

/// 通过轻量级 API 调用验证 Setup Token。
pub struct TokenTester;

impl TokenTester {
    pub fn new() -> Self {
        Self
    }

    /// 通过发送最小消息请求验证 Setup Token 有效性。
    pub async fn test_token(
        &self,
        token: &str,
        proxy_url: &str,
        canonical_env: &Value,
    ) -> Result<(), AppError> {
        let env: CanonicalEnvData =
            serde_json::from_value(canonical_env.clone()).unwrap_or_default();
        let version = if env.version.is_empty() {
            crate::config::CLAUDE_CODE_VERSION
        } else {
            &env.version
        };
        let stainless_os = match env.platform.as_str() {
            "darwin" => "Mac OS X",
            "win32" => "Windows",
            _ => "Linux",
        };

        let body = serde_json::json!({
            "model": "claude-haiku-4-5-20251001",
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}]
        });

        let client = make_request_client(proxy_url);

        let resp = client
            .post("https://api.anthropic.com/v1/messages?beta=true")
            .header("Authorization", format!("Bearer {}", token))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,context-management-2025-06-27,prompt-caching-scope-2026-01-05")
            .header("anthropic-dangerous-direct-browser-access", "true")
            .header("User-Agent", format!("claude-cli/{} (external, cli)", version))
            .header("x-app", "cli")
            // 不能要 br/gzip/deflate: 全局 reqwest 没启 gzip feature (gateway 透传需要保留压缩字节),
            // 这里要了上游会回压缩字节, resp.text() 当 UTF-8 读 → 乱码。强制 identity 拿到明文。
            .header("accept-encoding", "identity")
            .header("accept-language", "*")
            .header("sec-fetch-mode", "cors")
            .header("X-Stainless-Lang", "js")
            .header("X-Stainless-Package-Version", "0.81.0")
            .header("X-Stainless-OS", stainless_os)
            .header("X-Stainless-Arch", &env.arch)
            .header("X-Stainless-Runtime", "node")
            .header("X-Stainless-Runtime-Version", &env.node_version)
            .header("X-Stainless-Retry-Count", "0")
            .header("X-Stainless-Timeout", "600")
            .json(&body)
            .send()
            .await
            .map_err(|e| AppError::Internal(format!("request failed: {:?}", e)))?;

        if resp.status() != 200 {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(AppError::Internal(format!(
                "token test failed: status {} {}",
                status, text
            )));
        }
        Ok(())
    }
}

/// 使用 refresh token 刷新 OAuth access token。
pub async fn refresh_oauth_token(
    refresh_token: &str,
    proxy_url: &str,
) -> Result<RefreshedOAuthTokens, AppError> {
    let client = make_request_client(proxy_url);
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": OAUTH_CLIENT_ID,
        "scope": OAUTH_SCOPES.join(" "),
    });

    let resp = client
        .post(OAUTH_TOKEN_URL)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/plain, */*")
        .header("User-Agent", "axios/1.13.6")
        .json(&body)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("oauth refresh request failed: {}", e)))?;

    if resp.status() != 200 {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        // 错误归因 (供调用方区分临时 vs 永久):
        //   4xx (含 401 invalid_grant / 400 invalid_request) → BadRequest = 永久, 凭证真的废了, 写 auth_error
        //   429 → TooManyRequests = 临时, 上游限流, 不写 auth_error
        //   5xx / 网络层 → Internal = 临时, 上游抖动, 不写 auth_error
        // 修 Y1: 上游 body 走 sanitize, 防 Bearer / sk- 等敏感字段进 error 日志
        let safe_text = sanitize_upstream_error_body(&text);
        return Err(match status.as_u16() {
            429 => AppError::TooManyRequests(format!(
                "oauth refresh rate-limited: status {} {}",
                status, safe_text
            )),
            400..=499 => AppError::BadRequest(format!(
                "oauth refresh rejected: status {} {}",
                status, safe_text
            )),
            _ => AppError::Internal(format!(
                "oauth refresh failed: status {} {}",
                status, safe_text
            )),
        });
    }

    let data: OAuthRefreshResponse = resp
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("oauth refresh parse failed: {}", e)))?;

    let expires_in = if data.expires_in > 0 {
        data.expires_in
    } else {
        3600
    };
    let expires_at = Utc::now() + chrono::Duration::seconds(expires_in);

    Ok(RefreshedOAuthTokens {
        access_token: data.access_token,
        refresh_token: if data.refresh_token.is_empty() {
            refresh_token.to_string()
        } else {
            data.refresh_token
        },
        expires_at,
    })
}

/// 从 Anthropic OAuth API 获取账号用量数据。
pub async fn fetch_usage(token: &str, proxy_url: &str) -> Result<Value, AppError> {
    let client = make_request_client(proxy_url);

    let resp = client
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", format!("Bearer {}", token))
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("anthropic-beta", "oauth-2025-04-20")
        .header(
            "User-Agent",
            format!("claude-code/{}", crate::config::CLAUDE_CODE_VERSION),
        )
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("usage request failed: {}", e)))?;

    let status = resp.status();
    if status != 200 {
        let text = resp.text().await.unwrap_or_default();
        let safe_text = sanitize_upstream_error_body(&text);
        return Err(match status.as_u16() {
            401 | 403 => AppError::BadRequest(format!(
                "usage fetch failed: status {} — token may be expired or invalid: {}",
                status, safe_text
            )),
            429 => AppError::TooManyRequests(format!(
                "usage endpoint rate limited (429), try again later: {}",
                safe_text
            )),
            _ => AppError::Internal(format!("usage fetch failed: status {} {}", status, safe_text)),
        });
    }

    let data: Value = resp
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("usage parse failed: {}", e)))?;
    Ok(data)
}
