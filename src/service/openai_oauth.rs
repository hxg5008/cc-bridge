//! OpenAI OAuth (auth.openai.com PKCE) 浏览器授权流程。
//!
//! 与 Claude 的 OAuth (oauth_flow.rs) 平级独立的模块, 因为 OpenAI 用不同的:
//!   - ClientID (Codex CLI 的 app_EMoamEEZ73f0CkXaXp7hrann)
//!   - 端点 (auth.openai.com)
//!   - PKCE 实现细节 (verifier=64 字节 hex, 不是 base64url)
//!   - id_token JWT 解析获取 chatgpt_account_id / chatgpt_user_id / organization_id

use std::collections::HashMap;
use std::time::{Duration, Instant};
// parking_lot::Mutex 替 std::sync::Mutex (跟 N1/R3 同款修复, 漏掉了 OpenAI 路径):
// std Mutex 持锁时 panic 中毒会让 OpenAI OAuth 子系统永久挂掉, parking_lot 不会。
use parking_lot::Mutex;

use base64::Engine;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::AppError;

// OAuth 常量 (从 sub2api/openai_oauth_service.go 移植)
const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const OPENAI_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OPENAI_DEFAULT_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const OPENAI_DEFAULT_SCOPE: &str = "openid profile email offline_access";
const OPENAI_REFRESH_SCOPE: &str = "openid profile email";
const OPENAI_CLI_USER_AGENT: &str = "codex-cli/0.91.0";

const SESSION_TTL: Duration = Duration::from_secs(30 * 60);

// ---------------------------------------------------------------------------
// PKCE 工具 (注意: OpenAI 的 verifier 是 64 字节 hex, 与 Claude 的 base64url 不同)
// ---------------------------------------------------------------------------

fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill(&mut buf[..]);
    hex::encode(&buf)
}

/// OpenAI 的 code_verifier: 64 字节随机 → hex 编码 (128 字符)
fn generate_code_verifier() -> String {
    random_hex(64)
}

/// OpenAI 的 code_challenge: SHA256(verifier) → base64url
fn generate_code_challenge(verifier: &str) -> String {
    let hash = Sha256::digest(verifier.as_bytes());
    b64url(&hash)
}

/// state: 32 字节随机 → hex
fn generate_state() -> String {
    random_hex(32)
}

fn generate_session_id() -> String {
    random_hex(16)
}

// ---------------------------------------------------------------------------
// 会话存储 (与 Claude oauth_flow 同模式)
// ---------------------------------------------------------------------------

struct OpenAISession {
    state: String,
    code_verifier: String,
    redirect_uri: String,
    proxy_url: String,
    created_at: Instant,
}

struct SessionStore {
    sessions: Mutex<HashMap<String, OpenAISession>>,
}

impl SessionStore {
    fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }
    fn set(&self, id: &str, s: OpenAISession) {
        let mut map = self.sessions.lock();
        map.retain(|_, v| v.created_at.elapsed() < SESSION_TTL);
        map.insert(id.to_string(), s);
    }
    fn take(&self, id: &str) -> Option<OpenAISession> {
        let mut map = self.sessions.lock();
        map.remove(id)
    }
}

// ---------------------------------------------------------------------------
// 请求 / 响应
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct OpenAIGenerateAuthUrlRequest {
    #[serde(default)]
    pub redirect_uri: Option<String>,
    #[serde(default)]
    pub proxy_url: Option<String>,
}

#[derive(Serialize)]
pub struct OpenAIGenerateAuthUrlResponse {
    pub auth_url: String,
    pub session_id: String,
    pub state: String,
}

#[derive(Deserialize)]
pub struct OpenAIExchangeCodeRequest {
    pub session_id: String,
    pub code: String,
    /// 可选: 客户端粘回来时附带 state 校验
    #[serde(default)]
    pub state: Option<String>,
}

#[derive(Serialize)]
pub struct OpenAIExchangeCodeResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: String,
    pub expires_in: i64,
    pub expires_at: i64,
    pub email: String,
    pub chatgpt_account_id: String,
    pub chatgpt_user_id: String,
    pub plan_type: String,
    pub organization_id: String,
    /// 来自 chatgpt.com /backend-api/accounts/check 的订阅过期时间 (RFC3339)。
    /// id_token 不一定带,enrich 步骤补全。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subscription_expires_at: String,
    /// 隐私设置 (`disable_openai_training` 的结果),用于 UI 显示和后续判断。
    /// 空字符串表示未尝试。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub privacy_mode: String,
}

#[derive(Deserialize)]
struct OpenAITokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    id_token: String,
    #[serde(default)]
    expires_in: i64,
}

// ---------------------------------------------------------------------------
// id_token JWT 解析 (不验证签名, 仅解码 payload)
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct IdTokenClaims {
    pub email: String,
    pub chatgpt_account_id: String,
    pub chatgpt_user_id: String,
    pub plan_type: String,
    pub organization_id: String,
}

/// 解析 OpenAI OAuth id_token 的 payload (JWT 第二段) 提取 email/account_id 等。
///
/// **安全警告 (修 Y6)**: 此函数**不验证 JWT 签名**, 只解码 base64 payload。
/// 调用方信任 payload 内容的前提是: id_token 来自可信信道 (HTTPS 直连
/// auth.openai.com 或经过可信 proxy)。如果 proxy_url 走了不可信的 SOCKS / HTTP
/// 代理, 攻击者可 MITM 替换 id_token 注入伪造的 organization_id / email,
/// 导致后续该账号的 chatgpt-account-id header 全部错误 → 用户流量被路由到
/// 错的 OpenAI 组织。
///
/// 商用部署必须保证: PROXY_URL (.env 或 admin UI 设置) 只用于 HTTPS 端点 +
/// 来源可信。**绝不把不受控的开放代理填进去**。
fn parse_id_token(id_token: &str) -> IdTokenClaims {
    let parts: Vec<&str> = id_token.split('.').collect();
    if parts.len() < 2 {
        return IdTokenClaims::default();
    }
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let payload = match engine.decode(parts[1]) {
        Ok(b) => b,
        Err(_) => return IdTokenClaims::default(),
    };
    let json: serde_json::Value = match serde_json::from_slice(&payload) {
        Ok(v) => v,
        Err(_) => return IdTokenClaims::default(),
    };
    let mut out = IdTokenClaims::default();
    if let Some(s) = json.get("email").and_then(|v| v.as_str()) {
        out.email = s.to_string();
    }
    // 嵌套字段在 "https://api.openai.com/auth"
    if let Some(auth) = json.get("https://api.openai.com/auth") {
        if let Some(s) = auth.get("chatgpt_account_id").and_then(|v| v.as_str()) {
            out.chatgpt_account_id = s.to_string();
        }
        if let Some(s) = auth.get("chatgpt_user_id").and_then(|v| v.as_str()) {
            out.chatgpt_user_id = s.to_string();
        }
        if let Some(s) = auth.get("chatgpt_plan_type").and_then(|v| v.as_str()) {
            out.plan_type = s.to_string();
        }
        // organizations[] 数组里取 default
        if let Some(arr) = auth.get("organizations").and_then(|v| v.as_array()) {
            for org in arr {
                if org
                    .get("is_default")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    if let Some(s) = org.get("id").and_then(|v| v.as_str()) {
                        out.organization_id = s.to_string();
                        break;
                    }
                }
            }
            // 没有 default 就取第一个
            if out.organization_id.is_empty() {
                if let Some(first) = arr.first() {
                    if let Some(s) = first.get("id").and_then(|v| v.as_str()) {
                        out.organization_id = s.to_string();
                    }
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// OpenAIOAuthService
// ---------------------------------------------------------------------------

pub struct OpenAIOAuthService {
    store: SessionStore,
}

impl OpenAIOAuthService {
    pub fn new() -> Self {
        Self {
            store: SessionStore::new(),
        }
    }

    /// 生成浏览器 OAuth 授权 URL。
    pub fn generate_auth_url(
        &self,
        req: &OpenAIGenerateAuthUrlRequest,
    ) -> OpenAIGenerateAuthUrlResponse {
        let redirect_uri = req
            .redirect_uri
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| OPENAI_DEFAULT_REDIRECT_URI.to_string());
        let proxy_url = req.proxy_url.clone().unwrap_or_default();

        let state = generate_state();
        let code_verifier = generate_code_verifier();
        let code_challenge = generate_code_challenge(&code_verifier);
        let session_id = generate_session_id();

        self.store.set(
            &session_id,
            OpenAISession {
                state: state.clone(),
                code_verifier,
                redirect_uri: redirect_uri.clone(),
                proxy_url,
                created_at: Instant::now(),
            },
        );

        let encoded_redirect = percent_encode(&redirect_uri);
        let encoded_scope = OPENAI_DEFAULT_SCOPE.replace(' ', "+");
        let auth_url = format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}\
             &code_challenge={}&code_challenge_method=S256\
             &id_token_add_organizations=true&codex_cli_simplified_flow=true",
            OPENAI_AUTHORIZE_URL,
            OPENAI_CLIENT_ID,
            encoded_redirect,
            encoded_scope,
            state,
            code_challenge
        );

        OpenAIGenerateAuthUrlResponse {
            auth_url,
            session_id,
            state,
        }
    }

    /// 交换 code 拿 token (从浏览器 callback URL 复制 code 回粘后调用)。
    pub async fn exchange_code(
        &self,
        req: &OpenAIExchangeCodeRequest,
    ) -> Result<OpenAIExchangeCodeResponse, AppError> {
        let session = self
            .store
            .take(&req.session_id)
            .ok_or_else(|| AppError::BadRequest("invalid or expired session_id".into()))?;
        if session.created_at.elapsed() >= SESSION_TTL {
            return Err(AppError::BadRequest("session expired".into()));
        }

        // state 校验 (常时比较)
        if let Some(s) = req.state.as_ref().filter(|s| !s.is_empty()) {
            if !ct_eq(s.as_bytes(), session.state.as_bytes()) {
                return Err(AppError::BadRequest("state mismatch".into()));
            }
        }

        // POST /oauth/token (form-urlencoded)
        let form = [
            ("grant_type", "authorization_code"),
            ("client_id", OPENAI_CLIENT_ID),
            ("code", req.code.trim()),
            ("redirect_uri", session.redirect_uri.as_str()),
            ("code_verifier", session.code_verifier.as_str()),
        ];

        let client = crate::tlsfp::make_request_client(&session.proxy_url);
        let resp = client
            .post(OPENAI_TOKEN_URL)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .header("User-Agent", OPENAI_CLI_USER_AGENT)
            .form(&form)
            .send()
            .await
            .map_err(|e| AppError::Internal(format!("openai token request failed: {}", e)))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(AppError::Internal(format!(
                "openai token exchange failed: {} {}",
                status, text
            )));
        }
        let token: OpenAITokenResponse = resp
            .json()
            .await
            .map_err(|e| AppError::Internal(format!("openai token parse failed: {}", e)))?;

        let claims = parse_id_token(&token.id_token);
        let expires_in = if token.expires_in > 0 {
            token.expires_in
        } else {
            3600
        };
        let expires_at = chrono::Utc::now().timestamp() + expires_in;

        let mut resp = OpenAIExchangeCodeResponse {
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            id_token: token.id_token,
            expires_in,
            expires_at,
            email: claims.email,
            chatgpt_account_id: claims.chatgpt_account_id,
            chatgpt_user_id: claims.chatgpt_user_id,
            plan_type: claims.plan_type,
            organization_id: claims.organization_id,
            subscription_expires_at: String::new(),
            privacy_mode: String::new(),
        };
        enrich_token_info(&mut resp, &session.proxy_url).await;
        Ok(resp)
    }

    /// 用 refresh_token 刷新 access_token (token 自动刷新机制用)。
    pub async fn refresh_token(
        &self,
        refresh_token: &str,
        proxy_url: &str,
    ) -> Result<OpenAIExchangeCodeResponse, AppError> {
        if refresh_token.is_empty() {
            return Err(AppError::BadRequest("refresh_token is empty".into()));
        }
        let form = [
            ("grant_type", "refresh_token"),
            ("client_id", OPENAI_CLIENT_ID),
            ("refresh_token", refresh_token),
            ("scope", OPENAI_REFRESH_SCOPE),
        ];

        let client = crate::tlsfp::make_request_client(proxy_url);
        let resp = client
            .post(OPENAI_TOKEN_URL)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .header("User-Agent", OPENAI_CLI_USER_AGENT)
            .form(&form)
            .send()
            .await
            .map_err(|e| AppError::Internal(format!("openai refresh request failed: {}", e)))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(AppError::Internal(format!(
                "openai refresh failed: {} {}",
                status, text
            )));
        }
        let token: OpenAITokenResponse = resp
            .json()
            .await
            .map_err(|e| AppError::Internal(format!("openai refresh parse failed: {}", e)))?;

        // 刷新返回的 refresh_token 可能为空, 此时复用旧的
        let new_refresh = if token.refresh_token.is_empty() {
            refresh_token.to_string()
        } else {
            token.refresh_token
        };
        let claims = parse_id_token(&token.id_token);
        let expires_in = if token.expires_in > 0 {
            token.expires_in
        } else {
            3600
        };
        let expires_at = chrono::Utc::now().timestamp() + expires_in;

        let mut response = OpenAIExchangeCodeResponse {
            access_token: token.access_token,
            refresh_token: new_refresh,
            id_token: token.id_token,
            expires_in,
            expires_at,
            email: claims.email,
            chatgpt_account_id: claims.chatgpt_account_id,
            chatgpt_user_id: claims.chatgpt_user_id,
            plan_type: claims.plan_type,
            organization_id: claims.organization_id,
            subscription_expires_at: String::new(),
            privacy_mode: String::new(),
        };
        enrich_token_info(&mut response, proxy_url).await;
        Ok(response)
    }
}

/// 调一次 chatgpt.com /backend-api/accounts/check 拿真实 plan_type/email/subscription,
/// 顺便尝试 PATCH 关训练数据共享。失败均不阻断,只在响应里留一些字段。
async fn enrich_token_info(resp: &mut OpenAIExchangeCodeResponse, proxy_url: &str) {
    if resp.access_token.trim().is_empty() {
        return;
    }
    let info = crate::service::openai_privacy::fetch_chatgpt_account_info(
        &resp.access_token,
        proxy_url,
        &resp.organization_id,
    )
    .await;
    if let Some(info) = info {
        if !info.plan_type.is_empty() {
            resp.plan_type = info.plan_type;
        }
        if resp.email.is_empty() && !info.email.is_empty() {
            resp.email = info.email;
        }
        if !info.subscription_expires_at.is_empty() {
            resp.subscription_expires_at = info.subscription_expires_at;
        }
    }
    let mode = crate::service::openai_privacy::disable_openai_training(
        &resp.access_token,
        proxy_url,
    )
    .await;
    resp.privacy_mode = mode.as_str().to_string();
}

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

/// 常时字节比较 (state 校验)
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
