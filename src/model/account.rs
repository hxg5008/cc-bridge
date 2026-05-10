use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

mod optional_timestamp_millis {
    use chrono::{DateTime, TimeZone, Utc};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &Option<DateTime<Utc>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(dt) => serializer.serialize_i64(dt.timestamp_millis()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<DateTime<Utc>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Option::<i64>::deserialize(deserializer)?;
        value
            .map(|ms| {
                Utc.timestamp_millis_opt(ms)
                    .single()
                    .ok_or_else(|| serde::de::Error::custom("invalid timestamp millis"))
            })
            .transpose()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AccountStatus {
    Active,
    Error,
    Disabled,
}

impl Default for AccountStatus {
    fn default() -> Self {
        Self::Active
    }
}

impl std::fmt::Display for AccountStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => write!(f, "active"),
            Self::Error => write!(f, "error"),
            Self::Disabled => write!(f, "disabled"),
        }
    }
}

impl From<String> for AccountStatus {
    fn from(s: String) -> Self {
        match s.as_str() {
            "active" => Self::Active,
            "error" => Self::Error,
            "disabled" => Self::Disabled,
            _ => Self::Active,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum BillingMode {
    Strip,
    Rewrite,
}

impl Default for BillingMode {
    fn default() -> Self {
        Self::Strip
    }
}

impl std::fmt::Display for BillingMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Strip => write!(f, "strip"),
            Self::Rewrite => write!(f, "rewrite"),
        }
    }
}

impl From<String> for BillingMode {
    fn from(s: String) -> Self {
        match s.as_str() {
            "rewrite" => Self::Rewrite,
            _ => Self::Strip,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AccountAuthType {
    SetupToken,
    Oauth,
}

impl Default for AccountAuthType {
    fn default() -> Self {
        Self::SetupToken
    }
}

impl std::fmt::Display for AccountAuthType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SetupToken => write!(f, "setup_token"),
            Self::Oauth => write!(f, "oauth"),
        }
    }
}

impl From<String> for AccountAuthType {
    fn from(s: String) -> Self {
        match s.as_str() {
            "oauth" => Self::Oauth,
            _ => Self::SetupToken,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: i64,
    pub name: String,
    pub email: String,
    pub status: AccountStatus,
    #[serde(default)]
    pub auth_type: AccountAuthType,
    #[serde(default)]
    pub setup_token: String,
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default, with = "optional_timestamp_millis")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth_refreshed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub auth_error: String,
    #[serde(default)]
    pub proxy_url: String,
    pub device_id: String,
    pub canonical_env: Value,
    #[serde(rename = "canonical_prompt_env")]
    pub canonical_prompt: Value,
    pub canonical_process: Value,
    pub billing_mode: BillingMode,
    /// OAuth account UUID（强烈推荐填写，用于遥测改写）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_uuid: Option<String>,
    /// OAuth organization UUID（强烈推荐填写，用于遥测改写）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_uuid: Option<String>,
    /// 订阅类型：max / pro / team / enterprise（强烈推荐填写，用于遥测改写）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription_type: Option<String>,
    #[serde(default = "default_concurrency")]
    pub concurrency: i32,
    #[serde(default = "default_priority")]
    pub priority: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limited_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit_reset_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub disable_reason: String,
    /// 是否启用自动遥测。
    #[serde(default)]
    pub auto_telemetry: bool,
    /// 累计发送的遥测请求次数。
    #[serde(default)]
    pub telemetry_count: i64,
    /// 实验性：开启后剥离 redact-thinking-2026-02-12 beta token，让 thinking 正文回流。
    /// Anthropic 可能反 fingerprint，建议仅在测试账号开启。每个账号独立控制。
    #[serde(default)]
    pub experimental_reveal_thinking: bool,
    /// 1h 缓存 TTL 注入：开启后，对发往 /v1/messages 的请求体中已有的
    /// ephemeral cache_control 块强制写入 ttl="1h"，仅修改已存在的块，不新增缓存断点。
    /// 移植自 sub2api v0.1.121。每个账号独立控制，建议先在 1-2 个测试账号开启验证。
    #[serde(default)]
    pub enable_cache_ttl_1h_injection: bool,
    /// SessionKey from claude.ai cookie. Used for automatic OAuth recovery
    /// when refresh_token is revoked. Empty string = no recovery available.
    /// 明文存储；任何能 SELECT accounts 表的人能拿到完整登录凭证，请妥善保护 DB 访问权限。
    #[serde(default)]
    pub session_key: String,
    #[serde(default)]
    pub usage_data: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_fetched_at: Option<DateTime<Utc>>,
    /// 平台标识: "claude" / "openai" / 未来扩展。默认 "claude" 向后兼容。
    #[serde(default = "default_platform")]
    pub platform: String,
    /// 平台特有数据 (openai_passthrough / ua_override / chatgpt_account_id 等)。
    #[serde(default)]
    pub extra: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn default_platform() -> String {
    "claude".to_string()
}

fn default_concurrency() -> i32 {
    3
}
fn default_priority() -> i32 {
    50
}

impl Account {
    pub fn is_schedulable(&self) -> bool {
        self.status == AccountStatus::Active
    }

    pub fn has_valid_oauth_access_token(&self, buffer_seconds: i64) -> bool {
        if self.auth_type != AccountAuthType::Oauth || self.access_token.is_empty() {
            return false;
        }
        self.expires_at
            .map(|expires_at| expires_at > Utc::now() + chrono::Duration::seconds(buffer_seconds))
            .unwrap_or(false)
    }
}

/// 存储 20+ 维度的环境指纹数据。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CanonicalEnvData {
    pub platform: String,
    pub platform_raw: String,
    pub arch: String,
    pub node_version: String,
    pub terminal: String,
    pub package_managers: String,
    pub runtimes: String,
    #[serde(default)]
    pub is_running_with_bun: bool,
    #[serde(default)]
    pub is_ci: bool,
    #[serde(default)]
    pub is_claubbit: bool,
    #[serde(default)]
    pub is_claude_code_remote: bool,
    #[serde(default)]
    pub is_local_agent_mode: bool,
    #[serde(default)]
    pub is_conductor: bool,
    #[serde(default)]
    pub is_github_action: bool,
    #[serde(default)]
    pub is_claude_code_action: bool,
    #[serde(default)]
    pub is_claude_ai_auth: bool,
    pub version: String,
    pub version_base: String,
    pub build_time: String,
    pub deployment_environment: String,
    pub vcs: String,
}

/// 系统提示词中的环境改写数据。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CanonicalPromptEnvData {
    pub platform: String,
    pub shell: String,
    pub os_version: String,
    pub working_dir: String,
}

/// 硬件指纹配置。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CanonicalProcessData {
    pub constrained_memory: i64,
    pub rss_range: [i64; 2],
    pub heap_total_range: [i64; 2],
    pub heap_used_range: [i64; 2],
    #[serde(default)]
    pub external_range: [i64; 2],
    #[serde(default)]
    pub array_buffers_range: [i64; 2],
}
