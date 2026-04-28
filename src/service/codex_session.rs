//! OpenAI/Codex 上游 session 标识符派生。
//!
//! ChatGPT 内部 Codex 端点上有两层 session 概念:
//!
//! 1. **粘性会话 (sticky session)** — 同一个会话连续请求要落到同一个 OAuth 账号上,以
//!    复用 prompt cache、降低 5h 窗口消耗。本模块负责从请求体派生稳定 seed,然后
//!    通过 [`sticky_session_cache_key`] 转成 cache key 给 [`AccountService`] 用。
//!
//! 2. **session_id / conversation_id 头** — 上游会用这两个头做 prompt cache 命中
//!    判定。同一会话必须发相同的 `session_id` (UUID 形状) 才能命中 cache。我们用
//!    [`isolate_session_id`] + [`generate_session_uuid`] 把同一 seed 转成稳定的
//!    UUID,同时用 api_token id 做隔离,防止跨用户碰撞。
//!
//! 移植自 sub2api `openai_content_session_seed.go` / `openai_compat_prompt_cache_key.go` /
//! `openai_gateway_service.go::isolateOpenAISessionID`。

use serde_json::Value;
use sha2::{Digest, Sha256};
use xxhash_rust::xxh64::Xxh64;

const CONTENT_SESSION_SEED_PREFIX: &str = "compat_cs_";
const STICKY_CACHE_KEY_PREFIX: &str = "openai:";

/// 派生稳定的 session seed。
///
/// 优先级:
///   1. client 显式传的 `prompt_cache_key`
///   2. 否则按 model + tools/functions + instructions + system + first_user 派生
///
/// 同一会话连续请求应得到同一 seed (内容种子忽略 reasoning/历史 message 的尾部增量)。
pub fn derive_session_seed(body: &Value, client_prompt_cache_key: Option<&str>) -> String {
    if let Some(k) = client_prompt_cache_key {
        let trimmed = k.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    derive_content_session_seed(body)
}

/// 内容种子: 从请求体中提取相对稳定的字段拼接,作为同一会话的标识。
///
/// 移植自 deriveOpenAIContentSessionSeed (openai_content_session_seed.go:19)。
fn derive_content_session_seed(body: &Value) -> String {
    let mut buf = String::new();

    if let Some(model) = body.get("model").and_then(|v| v.as_str()) {
        if !model.is_empty() {
            buf.push_str("model=");
            buf.push_str(model);
        }
    }

    if let Some(tools) = body.get("tools") {
        if tools.is_array() && !is_empty_array(tools) {
            push_seed_field(&mut buf, "tools", tools);
        }
    }
    if let Some(functions) = body.get("functions") {
        if functions.is_array() && !is_empty_array(functions) {
            push_seed_field(&mut buf, "functions", functions);
        }
    }
    if let Some(instr) = body.get("instructions").and_then(|v| v.as_str()) {
        if !instr.is_empty() {
            buf.push_str("|instructions=");
            buf.push_str(instr);
        }
    }

    let mut first_user_captured = false;

    if let Some(messages) = body.get("messages").and_then(|v| v.as_array()) {
        for msg in messages {
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
            let content = msg.get("content");
            match role {
                "system" | "developer" => {
                    if let Some(c) = content {
                        push_seed_field(&mut buf, "system", c);
                    }
                }
                "user" => {
                    if !first_user_captured {
                        if let Some(c) = content {
                            push_seed_field(&mut buf, "first_user", c);
                        }
                        first_user_captured = true;
                    }
                }
                _ => {}
            }
        }
    } else if let Some(input) = body.get("input") {
        match input {
            Value::String(s) => {
                buf.push_str("|input=");
                buf.push_str(s);
            }
            Value::Array(items) => {
                for item in items {
                    let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("");
                    match role {
                        "system" | "developer" => {
                            if let Some(c) = item.get("content") {
                                push_seed_field(&mut buf, "system", c);
                            }
                        }
                        "user" => {
                            if !first_user_captured {
                                if let Some(c) = item.get("content") {
                                    push_seed_field(&mut buf, "first_user", c);
                                }
                                first_user_captured = true;
                            }
                        }
                        _ => {}
                    }
                    if !first_user_captured {
                        if let Some(typ) = item.get("type").and_then(|v| v.as_str()) {
                            if typ == "input_text" {
                                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                                    if !text.is_empty() {
                                        buf.push_str("|first_user=");
                                        buf.push_str(text);
                                        first_user_captured = true;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    if buf.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(CONTENT_SESSION_SEED_PREFIX.len() + buf.len());
    out.push_str(CONTENT_SESSION_SEED_PREFIX);
    out.push_str(&buf);
    out
}

fn is_empty_array(v: &Value) -> bool {
    matches!(v, Value::Array(a) if a.is_empty())
}

fn push_seed_field(buf: &mut String, name: &str, value: &Value) {
    if !buf.is_empty() {
        buf.push('|');
    }
    buf.push_str(name);
    buf.push('=');
    buf.push_str(&normalize_seed_json(value));
}

fn normalize_seed_json(v: &Value) -> String {
    if let Value::String(s) = v {
        return s.clone();
    }
    serde_json::to_string(v).unwrap_or_default()
}

/// 粘性会话 cache key,加 `openai:` 前缀避免和 Claude 粘性会话冲突。
pub fn sticky_session_cache_key(seed: &str) -> String {
    let trimmed = seed.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut hasher = Xxh64::new(0);
    hasher.update(trimmed.as_bytes());
    format!("{}{:016x}", STICKY_CACHE_KEY_PREFIX, hasher.digest())
}

/// 上游 session_id 隔离: xxhash(api_token_id 前缀 + seed) → 64-bit hex。
///
/// 移植自 isolateOpenAISessionID (openai_gateway_service.go:880)。
/// 不同 api_token 用相同 raw_seed 必须产生不同结果,防止跨用户 prompt cache 串扰。
pub fn isolate_session_id(api_token_id: i64, raw_seed: &str) -> String {
    let trimmed = raw_seed.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut hasher = Xxh64::new(0);
    let prefix = format!("k{}:", api_token_id);
    hasher.update(prefix.as_bytes());
    hasher.update(trimmed.as_bytes());
    format!("{:016x}", hasher.digest())
}

/// 把任意 seed 转成稳定的 UUID v4 形状字符串。
///
/// 上游 `session_id` / `conversation_id` 头期望 UUID;同一 seed 必须永远得到同一个 UUID。
pub fn generate_session_uuid(seed: &str) -> String {
    let trimmed = seed.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let digest = Sha256::digest(trimmed.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // RFC 4122: version 4 (随机) + variant 10
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

/// 仅 OAuth + GPT-5 codex 系才自动注入 prompt_cache_key,避免误判 gpt-4o / claude-* 等。
///
/// 移植自 shouldAutoInjectPromptCacheKeyForCompat (openai_compat_prompt_cache_key.go:12)。
pub fn should_auto_inject_prompt_cache_key(model: &str) -> bool {
    let lower = model.trim().to_lowercase();
    if !lower.contains("gpt-5") && !lower.contains("codex") {
        return false;
    }
    matches!(
        crate::service::codex_transform::normalize_codex_model(&lower).as_str(),
        "gpt-5.4" | "gpt-5.3-codex" | "gpt-5.3-codex-spark"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn explicit_pck_takes_priority() {
        let body = json!({"model": "gpt-5.4", "messages": [{"role":"user","content":"hi"}]});
        let seed = derive_session_seed(&body, Some("my-pck"));
        assert_eq!(seed, "my-pck");
    }

    #[test]
    fn content_seed_is_stable() {
        let a = json!({
            "model": "gpt-5.4",
            "instructions": "hello",
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "first"},
                {"role": "assistant", "content": "answer"},
                {"role": "user", "content": "second turn"},
            ]
        });
        let b = json!({
            "model": "gpt-5.4",
            "instructions": "hello",
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "first"},
                {"role": "assistant", "content": "different answer"},
                {"role": "user", "content": "third turn"},
            ]
        });
        // 增量消息差异不影响 seed (只取 system + first_user)
        let s1 = derive_session_seed(&a, None);
        let s2 = derive_session_seed(&b, None);
        assert_eq!(s1, s2);
        assert!(s1.starts_with(CONTENT_SESSION_SEED_PREFIX));
    }

    #[test]
    fn responses_input_array_seed() {
        let body = json!({
            "model": "gpt-5.4",
            "input": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": [{"type":"input_text","text":"hello"}]}
            ]
        });
        let s = derive_session_seed(&body, None);
        assert!(s.contains("system="));
        assert!(s.contains("first_user="));
    }

    #[test]
    fn isolate_session_id_differs_across_tokens() {
        let a = isolate_session_id(1, "seed");
        let b = isolate_session_id(2, "seed");
        let c = isolate_session_id(1, "seed");
        assert_eq!(a, c, "same token + seed must be stable");
        assert_ne!(a, b, "different tokens must isolate");
    }

    #[test]
    fn session_uuid_format() {
        let u = generate_session_uuid("abc");
        assert_eq!(u.len(), 36);
        let bytes = u.as_bytes();
        assert_eq!(bytes[8], b'-');
        assert_eq!(bytes[13], b'-');
        assert_eq!(bytes[18], b'-');
        assert_eq!(bytes[23], b'-');
        // version 4 nibble at position 14
        assert_eq!(bytes[14], b'4');
        // variant 10xx at position 19 ∈ {8,9,a,b}
        assert!(matches!(bytes[19], b'8' | b'9' | b'a' | b'b'));
        // 同一 seed 稳定
        assert_eq!(u, generate_session_uuid("abc"));
    }

    #[test]
    fn sticky_key_has_prefix() {
        let k = sticky_session_cache_key("seed");
        assert!(k.starts_with("openai:"));
        assert_eq!(k.len(), "openai:".len() + 16);
    }

    #[test]
    fn should_auto_inject_only_for_supported_models() {
        assert!(should_auto_inject_prompt_cache_key("gpt-5.4"));
        assert!(should_auto_inject_prompt_cache_key("gpt-5.4-high"));
        assert!(should_auto_inject_prompt_cache_key("gpt-5.3-codex"));
        assert!(should_auto_inject_prompt_cache_key("gpt-5.3-codex-spark"));
        assert!(should_auto_inject_prompt_cache_key("codex"));
        assert!(!should_auto_inject_prompt_cache_key("gpt-4o"));
        assert!(!should_auto_inject_prompt_cache_key("claude-sonnet"));
        assert!(!should_auto_inject_prompt_cache_key(""));
    }

    #[test]
    fn empty_seed_returns_empty_keys() {
        assert_eq!(sticky_session_cache_key(""), "");
        assert_eq!(sticky_session_cache_key("   "), "");
        assert_eq!(isolate_session_id(1, ""), "");
        assert_eq!(generate_session_uuid(""), "");
    }
}
