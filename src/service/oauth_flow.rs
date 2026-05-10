use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
// parking_lot::Mutex 替 std::sync::Mutex (修 R3 同款 N1):
// std Mutex 持锁时 panic 中毒会让整个 OAuth 链路永久挂掉
use parking_lot::Mutex;

use base64::Engine;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::debug;

use crate::error::AppError;

// ---------------------------------------------------------------------------
// OAuth 常量
// ---------------------------------------------------------------------------

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
// SessionKey-based 自动 OAuth 用的 claude.ai 内部接口
const ORGANIZATIONS_URL: &str = "https://claude.ai/api/organizations";
const AUTHORIZE_API_URL: &str = "https://claude.ai/v1/oauth/{}/authorize";

const SCOPE_FULL: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const SCOPE_INFERENCE: &str = "user:inference";

/// 会话 TTL（30 分钟）。
const SESSION_TTL: Duration = Duration::from_secs(30 * 60);

/// Setup-Token 有效期（1 年）。
const SETUP_TOKEN_EXPIRES_IN: i64 = 365 * 24 * 60 * 60;

// ---------------------------------------------------------------------------
// PKCE 工具
// ---------------------------------------------------------------------------

/// base64url 编码（无填充）。
fn base64url_encode(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// 生成 PKCE code_verifier（32 字节随机 → 43 字符 base64url）。
fn generate_code_verifier() -> String {
    let mut buf = [0u8; 32];
    rand::thread_rng().fill(&mut buf);
    base64url_encode(&buf)
}

/// 计算 S256 code_challenge。
fn generate_code_challenge(verifier: &str) -> String {
    let hash = Sha256::digest(verifier.as_bytes());
    base64url_encode(&hash)
}

/// 生成随机 state。
fn generate_state() -> String {
    let mut buf = [0u8; 32];
    rand::thread_rng().fill(&mut buf);
    base64url_encode(&buf)
}

/// 生成 session_id。
fn generate_session_id() -> String {
    let mut buf = [0u8; 16];
    rand::thread_rng().fill(&mut buf);
    hex::encode(buf)
}

// ---------------------------------------------------------------------------
// 会话存储
// ---------------------------------------------------------------------------

struct OAuthSession {
    state: String,
    code_verifier: String,
    scope: String,
    proxy_url: String,
    created_at: Instant,
}

/// 内存级 OAuth 会话存储，带 TTL 自动清理。
///
/// 修 R3:
///   - 旧版只在 set 时 retain 过期, take 不清理。攻击者 generate-auth-url
///     刷出几十万 session 后不再调用 → 30 分钟内一直占内存 (单 session ~200B,
///     100w session ≈ 200MB)
///   - 新版加独立后台 GC tick (60s 一轮), 主动驱逐过期 session
///   - parking_lot Mutex 防 panic 中毒
struct SessionStore {
    sessions: Mutex<HashMap<String, OAuthSession>>,
}

/// 防止恶意刷 generate-auth-url 撑爆内存的硬上限。
/// 商用 200-300 DAU 正常情况下同时挂起的 session 不会超过几十个,
/// 上限 50K 留 1000x 余量同时把 OOM 风险盖死。
const SESSION_HARD_CAP: usize = 50_000;
const SESSION_GC_INTERVAL: Duration = Duration::from_secs(60);

impl SessionStore {
    fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn set(&self, id: &str, session: OAuthSession) {
        let mut map = self.sessions.lock();
        // 顺便清理过期会话
        map.retain(|_, s| s.created_at.elapsed() < SESSION_TTL);
        // 命中硬上限 → 拒绝写入 (防穷举攻击撑爆)
        if map.len() >= SESSION_HARD_CAP {
            tracing::warn!(
                "oauth session store hit hard cap ({}), rejecting new session", SESSION_HARD_CAP
            );
            return;
        }
        map.insert(id.to_string(), session);
    }

    fn take(&self, id: &str) -> Option<OAuthSession> {
        let mut map = self.sessions.lock();
        map.remove(id)
    }

    /// 后台 GC: 仅扫过期, 不动 hard cap (cap 已在 set 路径阻挡)。
    fn gc_once(&self) {
        let mut map = self.sessions.lock();
        let before = map.len();
        map.retain(|_, s| s.created_at.elapsed() < SESSION_TTL);
        if before != map.len() {
            tracing::debug!(before, after = map.len(), "oauth session store gc");
        }
    }
}

// ---------------------------------------------------------------------------
// 请求 / 响应
// ---------------------------------------------------------------------------

/// 生成授权 URL 的请求。
#[derive(Deserialize)]
pub struct GenerateAuthUrlRequest {
    pub proxy_url: Option<String>,
}

/// 生成授权 URL 的响应。
#[derive(Serialize)]
pub struct GenerateAuthUrlResponse {
    pub auth_url: String,
    pub session_id: String,
}

/// 交换 code 的请求。
#[derive(Deserialize)]
pub struct ExchangeCodeRequest {
    pub session_id: String,
    pub code: String,
}

/// 交换 code 的响应。
#[derive(Serialize)]
pub struct ExchangeCodeResponse {
    pub access_token: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub refresh_token: String,
    pub expires_in: i64,
    pub expires_at: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub scope: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub account_uuid: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub organization_uuid: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub email_address: String,
}

/// 平台 token exchange 原始响应。
#[derive(Deserialize)]
struct TokenExchangeResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_in: i64,
    #[serde(default)]
    scope: String,
    account: Option<TokenAccount>,
    organization: Option<TokenOrganization>,
}

#[derive(Deserialize)]
struct TokenAccount {
    uuid: String,
    #[serde(default)]
    email_address: String,
}

#[derive(Deserialize)]
struct TokenOrganization {
    uuid: String,
}

// ---------------------------------------------------------------------------
// SessionKey-based 自动 OAuth 请求 / 响应
// ---------------------------------------------------------------------------

/// 用 sessionKey 自动跑完三步 OAuth 流程的请求体。
#[derive(Deserialize)]
pub struct CookieAuthRequest {
    pub session_key: String,
    #[serde(default)]
    pub proxy_url: Option<String>,
    /// "full" (默认) 或 "inference" (Setup Token 模式)。
    #[serde(default)]
    pub scope: Option<String>,
}

/// CookieAuth 成功后的响应（与 ExchangeCodeResponse 字段一致）。
#[derive(Serialize, Clone)]
pub struct CookieAuthResponse {
    pub access_token: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub refresh_token: String,
    pub expires_in: i64,
    pub expires_at: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub scope: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub account_uuid: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub organization_uuid: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub email_address: String,
    /// 自动从 /api/organizations 推导出的订阅档位:
    /// "pro" | "max5" | "max20" | "free" | "" (未知)。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subscription_type: String,
}

#[derive(Deserialize)]
struct AuthorizeApiResponse {
    redirect_uri: String,
}

/// 简易 organization 字段反序列化。
#[derive(Deserialize)]
struct ClaudeOrganization {
    uuid: String,
    #[serde(default)]
    raven_type: Option<String>,
    /// 例如 "default_claude_ai" (Pro/Free), "default_claude_max_5x", "default_claude_max_20x"
    #[serde(default)]
    rate_limit_tier: String,
    /// 例如 ["chat", "claude_pro", "claude_code"] - 订阅 tag 与权限 tag 混在一起
    #[serde(default)]
    capabilities: Vec<String>,
}

// ---------------------------------------------------------------------------
// OAuthFlowService
// ---------------------------------------------------------------------------

/// 处理 OAuth 授权链接生成和 code 交换。
pub struct OAuthFlowService {
    store: Arc<SessionStore>,
}

impl OAuthFlowService {
    pub fn new() -> Self {
        let store = Arc::new(SessionStore::new());
        // 修 R3: 后台 GC 防止"set 过期 / take 未调"积压
        let weak = Arc::downgrade(&store);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SESSION_GC_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(store) = weak.upgrade() else { return; };
                store.gc_once();
            }
        });
        Self { store }
    }

    /// 生成 OAuth 授权 URL（完整 scope）。
    pub fn generate_auth_url(&self, req: &GenerateAuthUrlRequest) -> GenerateAuthUrlResponse {
        self.build_auth_url(SCOPE_FULL, req.proxy_url.as_deref().unwrap_or(""))
    }

    /// 生成 Setup-Token 授权 URL（仅 user:inference）。
    pub fn generate_setup_token_url(
        &self,
        req: &GenerateAuthUrlRequest,
    ) -> GenerateAuthUrlResponse {
        self.build_auth_url(SCOPE_INFERENCE, req.proxy_url.as_deref().unwrap_or(""))
    }

    /// 交换 code 获取 OAuth token（完整 scope）。
    pub async fn exchange_code(
        &self,
        req: &ExchangeCodeRequest,
    ) -> Result<ExchangeCodeResponse, AppError> {
        self.do_exchange(&req.session_id, &req.code, false).await
    }

    /// 交换 code 获取 Setup-Token。
    pub async fn exchange_setup_token_code(
        &self,
        req: &ExchangeCodeRequest,
    ) -> Result<ExchangeCodeResponse, AppError> {
        self.do_exchange(&req.session_id, &req.code, true).await
    }

    /// SessionKey-based 自动 OAuth: 三步走完, 直接返回 token 信息。
    /// 不依赖 SessionStore (无浏览器流程)。
    pub async fn cookie_auth(
        &self,
        req: &CookieAuthRequest,
    ) -> Result<CookieAuthResponse, AppError> {
        if req.session_key.trim().is_empty() {
            return Err(AppError::BadRequest("session_key 为空".into()));
        }
        let proxy = req.proxy_url.as_deref().unwrap_or("");
        let is_setup_token = matches!(req.scope.as_deref(), Some("inference"));
        let scope = if is_setup_token {
            SCOPE_INFERENCE
        } else {
            SCOPE_FULL
        };

        // Step 1: 拿 organization uuid + 自动探测的订阅档位
        let (org_uuid, subscription_type) =
            fetch_organization_uuid(&req.session_key, proxy).await?;
        debug!(
            "cookie_auth: 选中组织 {} (订阅: {})",
            org_uuid,
            if subscription_type.is_empty() { "unknown" } else { &subscription_type }
        );

        // Step 2: 生成 PKCE 三件套
        let state = generate_state();
        let code_verifier = generate_code_verifier();
        let code_challenge = generate_code_challenge(&code_verifier);

        // Step 3: 用 sessionKey + PKCE 拿 authorization code
        let raw_code = fetch_authorization_code(
            &req.session_key,
            &org_uuid,
            scope,
            &code_challenge,
            &state,
            proxy,
        )
        .await?;
        debug!("cookie_auth: 已拿到 authorization code");

        // Step 4: 交换 token
        let mut resp = exchange_for_token(
            &raw_code,
            &code_verifier,
            &state,
            proxy,
            is_setup_token,
            &org_uuid,
        )
        .await?;
        resp.subscription_type = subscription_type;
        Ok(resp)
    }

    // --- 内部实现 ---

    fn build_auth_url(&self, scope: &str, proxy_url: &str) -> GenerateAuthUrlResponse {
        let state = generate_state();
        let code_verifier = generate_code_verifier();
        let code_challenge = generate_code_challenge(&code_verifier);
        let session_id = generate_session_id();

        self.store.set(
            &session_id,
            OAuthSession {
                state: state.clone(),
                code_verifier,
                scope: scope.to_string(),
                proxy_url: proxy_url.to_string(),
                created_at: Instant::now(),
            },
        );

        let encoded_redirect = percent_encode(REDIRECT_URI);
        let encoded_scope = scope.replace(' ', "+");

        let auth_url = format!(
            "{}?code=true&client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
            AUTHORIZE_URL, CLIENT_ID, encoded_redirect, encoded_scope, code_challenge, state
        );

        debug!("generated auth URL for session {}", session_id);

        GenerateAuthUrlResponse {
            auth_url,
            session_id,
        }
    }

    async fn do_exchange(
        &self,
        session_id: &str,
        raw_code: &str,
        is_setup_token: bool,
    ) -> Result<ExchangeCodeResponse, AppError> {
        let session = self
            .store
            .take(session_id)
            .ok_or_else(|| AppError::BadRequest("invalid or expired session_id".into()))?;

        if session.created_at.elapsed() >= SESSION_TTL {
            return Err(AppError::BadRequest("session expired".into()));
        }

        // code 可能携带 state：code#state
        let (auth_code, code_state) = if let Some(idx) = raw_code.find('#') {
            (&raw_code[..idx], &raw_code[idx + 1..])
        } else {
            (raw_code, "")
        };

        // 构建 token exchange 请求体
        let mut body = serde_json::json!({
            "grant_type": "authorization_code",
            "code": auth_code,
            "redirect_uri": REDIRECT_URI,
            "client_id": CLIENT_ID,
            "code_verifier": session.code_verifier,
        });

        if !code_state.is_empty() {
            body["state"] = serde_json::Value::String(code_state.to_string());
        } else {
            body["state"] = serde_json::Value::String(session.state.clone());
        }

        if is_setup_token {
            body["expires_in"] = serde_json::json!(SETUP_TOKEN_EXPIRES_IN);
        }

        debug!("exchanging code for session {}", session_id);

        // 发送 token exchange 请求
        let client = crate::tlsfp::make_request_client(&session.proxy_url);
        let resp = client
            .post(TOKEN_URL)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| AppError::Internal(format!("token exchange request failed: {}", e)))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(AppError::Internal(format!(
                "token exchange failed: status {} {}",
                status, text
            )));
        }

        let token_resp: TokenExchangeResponse = resp
            .json()
            .await
            .map_err(|e| AppError::Internal(format!("token exchange parse failed: {}", e)))?;

        let expires_in = if token_resp.expires_in > 0 {
            token_resp.expires_in
        } else {
            3600
        };
        let expires_at = chrono::Utc::now().timestamp() + expires_in;

        Ok(ExchangeCodeResponse {
            access_token: token_resp.access_token,
            refresh_token: token_resp.refresh_token,
            expires_in,
            expires_at,
            scope: token_resp.scope,
            account_uuid: token_resp
                .account
                .as_ref()
                .map(|a| a.uuid.clone())
                .unwrap_or_default(),
            email_address: token_resp
                .account
                .as_ref()
                .map(|a| a.email_address.clone())
                .unwrap_or_default(),
            organization_uuid: token_resp
                .organization
                .as_ref()
                .map(|o| o.uuid.clone())
                .unwrap_or_default(),
        })
    }
}

/// 简易 percent-encode（仅编码 URL 不安全字符）。
fn percent_encode(input: &str) -> String {
    let mut result = String::with_capacity(input.len() * 3);
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(b as char);
            }
            _ => {
                result.push_str(&format!("%{:02X}", b));
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// SessionKey-based 自动 OAuth 私有辅助函数
// ---------------------------------------------------------------------------

/// 网络层重试次数 (claude.ai 在并发下偶发抽风, 重试 1-2 次能消化大部分瞬时抖动)。
/// 仅对 AppError::Internal (网络/解析层) 重试; AppError::BadRequest (401/403)
/// 视为真正失效, 不重试。
const NETWORK_RETRIES: u32 = 2;

/// Step 1: 用 sessionKey 拉 organizations 列表, 优先返回 raven_type=team 的 uuid,
/// 否则返回第一个组织。
/// 同时返回从 (rate_limit_tier, capabilities) 推导出的订阅档位
/// (pro / max5 / max20 / free / "")。
async fn fetch_organization_uuid(
    session_key: &str,
    proxy_url: &str,
) -> Result<(String, String), AppError> {
    let mut last_err: Option<AppError> = None;
    for attempt in 1u32..=(NETWORK_RETRIES + 1) {
        match fetch_organization_uuid_once(session_key, proxy_url).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                // sessionKey 真的失效 (401/403) → 不重试
                if matches!(&e, AppError::BadRequest(_)) {
                    return Err(e);
                }
                last_err = Some(e);
                if attempt <= NETWORK_RETRIES {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        800 * attempt as u64,
                    ))
                    .await;
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| AppError::Internal("unknown".into())))
}

async fn fetch_organization_uuid_once(
    session_key: &str,
    proxy_url: &str,
) -> Result<(String, String), AppError> {
    let client = crate::tlsfp::make_request_client(proxy_url);
    let resp = client
        .get(ORGANIZATIONS_URL)
        .header("Cookie", format!("sessionKey={}", session_key))
        .header("Accept", "application/json")
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
             AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36",
        )
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("organizations request failed: {}", e)))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(match status.as_u16() {
            401 | 403 => AppError::BadRequest(format!(
                "sessionKey 无效或已过期 (status {}): {}",
                status, text
            )),
            _ => AppError::Internal(format!("organizations failed: status {} {}", status, text)),
        });
    }

    let orgs: Vec<ClaudeOrganization> = resp
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("organizations parse failed: {}", e)))?;

    if orgs.is_empty() {
        return Err(AppError::BadRequest("账号下没有组织".into()));
    }

    // 优先 raven_type == "team", 否则第一个
    let chosen = orgs
        .iter()
        .find(|o| o.raven_type.as_deref() == Some("team"))
        .unwrap_or(&orgs[0]);
    let tier = normalize_claude_rate_limit_tier(&chosen.rate_limit_tier, &chosen.capabilities);
    Ok((chosen.uuid.clone(), tier))
}

/// 把 Anthropic 的 (rate_limit_tier, capabilities) 映射成稳定的订阅 ID:
/// pro / max5 / max20 / free / "" (未知)。
///
/// 为什么两个字段都要看:
///   单看 rate_limit_tier 区分不出 Pro 和 Free — 两者都可能返回
///   "default_claude_ai"。真正的订阅信息编码在 capabilities[] 里
///   (例如 "claude_pro" / "claude_max"), 与权限 tag (例如 "chat" /
///   "claude_code" / "console") 混在一起。
///
/// 决策逻辑 (capabilities 优先, 然后 tier):
///   capabilities 含 "claude_max":
///     - tier ~ max_20x  → max20
///     - tier ~ max_5x   → max5
///     - 其它            → max5 (兜底, 罕见)
///   capabilities 含 "claude_pro" → pro
///   都没有 → free
///
/// 未知 tier 兜底返回原始小写, 让 UI 仍能展示新出现的档位。
pub fn normalize_claude_rate_limit_tier(tier: &str, capabilities: &[String]) -> String {
    let tier_lower = tier.trim().to_ascii_lowercase();

    let mut has_max = false;
    let mut has_pro = false;
    for c in capabilities {
        match c.trim().to_ascii_lowercase().as_str() {
            "claude_max" => has_max = true,
            "claude_pro" => has_pro = true,
            _ => {}
        }
    }

    if has_max {
        return if tier_lower.contains("max_20x")
            || tier_lower.contains("max_20")
            || tier_lower.contains("max20")
        {
            "max20".to_string()
        } else if tier_lower.contains("max_5x")
            || tier_lower.contains("max_5")
            || tier_lower.contains("max5")
        {
            "max5".to_string()
        } else {
            // claude_max 但 tier 没 5x/20x 后缀, 兜底为 max5
            "max5".to_string()
        };
    }
    if has_pro {
        return "pro".to_string();
    }

    // 没有 claude_max / claude_pro tag — 按 tier 兜底
    if tier_lower.is_empty() {
        return String::new();
    }
    if tier_lower.contains("max_20x") || tier_lower.contains("max_20") || tier_lower.contains("max20") {
        return "max20".to_string();
    }
    if tier_lower.contains("max_5x") || tier_lower.contains("max_5") || tier_lower.contains("max5") {
        return "max5".to_string();
    }
    if tier_lower.contains("pro") {
        return "pro".to_string();
    }
    if tier_lower.contains("free")
        || tier_lower == "default_claude_ai"
        || tier_lower == "default"
    {
        return "free".to_string();
    }
    tier_lower
}

/// Step 2: 用 sessionKey 调 /v1/oauth/{org}/authorize 拿 redirect_uri 中的
/// authorization code (code+state 拼接为 "code#state")。
async fn fetch_authorization_code(
    session_key: &str,
    org_uuid: &str,
    scope: &str,
    code_challenge: &str,
    state: &str,
    proxy_url: &str,
) -> Result<String, AppError> {
    let mut last_err: Option<AppError> = None;
    for attempt in 1u32..=(NETWORK_RETRIES + 1) {
        match fetch_authorization_code_once(
            session_key,
            org_uuid,
            scope,
            code_challenge,
            state,
            proxy_url,
        )
        .await
        {
            Ok(v) => return Ok(v),
            Err(e) => {
                if matches!(&e, AppError::BadRequest(_)) {
                    return Err(e);
                }
                last_err = Some(e);
                if attempt <= NETWORK_RETRIES {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        800 * attempt as u64,
                    ))
                    .await;
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| AppError::Internal("unknown".into())))
}

async fn fetch_authorization_code_once(
    session_key: &str,
    org_uuid: &str,
    scope: &str,
    code_challenge: &str,
    state: &str,
    proxy_url: &str,
) -> Result<String, AppError> {
    let url = AUTHORIZE_API_URL.replacen("{}", org_uuid, 1);
    let body = serde_json::json!({
        "response_type": "code",
        "client_id": CLIENT_ID,
        "organization_uuid": org_uuid,
        "redirect_uri": REDIRECT_URI,
        "scope": scope,
        "state": state,
        "code_challenge": code_challenge,
        "code_challenge_method": "S256",
    });

    let client = crate::tlsfp::make_request_client(proxy_url);
    let resp = client
        .post(&url)
        .header("Cookie", format!("sessionKey={}", session_key))
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("Origin", "https://claude.ai")
        .header("Referer", "https://claude.ai/new")
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
             AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36",
        )
        .json(&body)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("authorize request failed: {}", e)))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(AppError::Internal(format!(
            "authorize failed: status {} {}",
            status, text
        )));
    }

    let parsed: AuthorizeApiResponse = resp
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("authorize parse failed: {}", e)))?;

    let redirect_uri = parsed.redirect_uri;
    // 解析 query string 中的 code / state
    let mut code: Option<String> = None;
    let mut resp_state: Option<String> = None;
    if let Some(q_pos) = redirect_uri.find('?') {
        for pair in redirect_uri[q_pos + 1..].split('&') {
            let mut iter = pair.splitn(2, '=');
            let k = iter.next().unwrap_or("");
            let v = iter.next().unwrap_or("");
            match k {
                "code" => code = Some(v.to_string()),
                "state" => resp_state = Some(v.to_string()),
                _ => {}
            }
        }
    }
    let code = code
        .ok_or_else(|| AppError::Internal(format!("redirect_uri 中无 code: {}", redirect_uri)))?;
    Ok(if let Some(s) = resp_state {
        format!("{}#{}", code, s)
    } else {
        code
    })
}

/// Step 3: POST /v1/oauth/token 交换 access_token (与 do_exchange 内部逻辑等价,
/// 但不依赖 SessionStore)。
async fn exchange_for_token(
    raw_code: &str,
    code_verifier: &str,
    fallback_state: &str,
    proxy_url: &str,
    is_setup_token: bool,
    fallback_org_uuid: &str,
) -> Result<CookieAuthResponse, AppError> {
    let (auth_code, code_state) = if let Some(idx) = raw_code.find('#') {
        (&raw_code[..idx], &raw_code[idx + 1..])
    } else {
        (raw_code, "")
    };

    let mut body = serde_json::json!({
        "grant_type": "authorization_code",
        "code": auth_code,
        "redirect_uri": REDIRECT_URI,
        "client_id": CLIENT_ID,
        "code_verifier": code_verifier,
    });
    if !code_state.is_empty() {
        body["state"] = serde_json::Value::String(code_state.to_string());
    } else if !fallback_state.is_empty() {
        body["state"] = serde_json::Value::String(fallback_state.to_string());
    }
    if is_setup_token {
        body["expires_in"] = serde_json::json!(SETUP_TOKEN_EXPIRES_IN);
    }

    // 注意: platform.claude.com/v1/oauth/token 对 TLS 指纹敏感, 要求 Chrome 家族;
    // craftls 是 Node.js 24.x 指纹, 该端点会回 403 "Request not allowed"。
    // 短期解法: cc-bridge 现有 craftls Node.js 指纹只能用于 claude.ai (Step 1+2)
    // 和 api.anthropic.com (推理), 但走不过 platform.claude.com 的 token 交换。
    // 长期需要扩展 craftls 支持 Chrome profile。
    let client = crate::tlsfp::make_request_client(proxy_url);
    let resp = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/plain, */*")
        .header("User-Agent", "axios/1.13.6")
        .json(&body)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("token exchange request failed: {}", e)))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(AppError::Internal(format!(
            "token exchange failed: status {} {}",
            status, text
        )));
    }

    let token_resp: TokenExchangeResponse = resp
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("token exchange parse failed: {}", e)))?;

    let expires_in = if token_resp.expires_in > 0 {
        token_resp.expires_in
    } else {
        3600
    };
    let expires_at = chrono::Utc::now().timestamp() + expires_in;

    let org_uuid_resp = token_resp
        .organization
        .as_ref()
        .map(|o| o.uuid.clone())
        .unwrap_or_default();

    Ok(CookieAuthResponse {
        access_token: token_resp.access_token,
        refresh_token: token_resp.refresh_token,
        expires_in,
        expires_at,
        scope: token_resp.scope,
        account_uuid: token_resp
            .account
            .as_ref()
            .map(|a| a.uuid.clone())
            .unwrap_or_default(),
        email_address: token_resp
            .account
            .as_ref()
            .map(|a| a.email_address.clone())
            .unwrap_or_default(),
        organization_uuid: if org_uuid_resp.is_empty() {
            fallback_org_uuid.to_string()
        } else {
            org_uuid_resp
        },
        subscription_type: String::new(),
    })
}
