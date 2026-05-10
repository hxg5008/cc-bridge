use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;
use subtle::ConstantTimeEq;

use crate::store::token_store::TokenStore;

/// 从请求头提取 API Key（x-api-key 或 Authorization: Bearer）
pub fn extract_key(req: &Request) -> String {
    let headers = req.headers();
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| {
            headers
                .get("Authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.strip_prefix("Bearer "))
                .map(|s| s.to_string())
        })
        .unwrap_or_default()
}

/// 提取客户端 IP: 优先用 X-Forwarded-For / X-Real-IP (反代场景),
/// 回退到 axum::extract::ConnectInfo 不可用时返回 None。
///
/// 修 N3: 旧版 unknown 时返回字面量 "unknown" 字符串, 所有失败计入同一桶,
/// 5 次失败把全部 admin 用户锁死。商业场景下 admin API 必须前置反代,
/// 反代必须注入 X-Forwarded-For; 否则 admin auth 拒绝服务 (返 None → 401)。
fn extract_client_ip(req: &Request) -> Option<String> {
    let headers = req.headers();
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        // X-Forwarded-For: client, proxy1, proxy2 → 取最左 (真实 client)
        if let Some(first) = xff.split(',').next() {
            let ip = first.trim();
            if !ip.is_empty() {
                return Some(ip.to_string());
            }
        }
    }
    if let Some(real) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
        let ip = real.trim();
        if !ip.is_empty() {
            return Some(ip.to_string());
        }
    }
    None
}

fn err_response(msg: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(serde_json::json!({"error": msg})),
    )
        .into_response()
}

fn locked_response(retry_after_secs: u64) -> Response {
    let mut resp = (
        StatusCode::TOO_MANY_REQUESTS,
        axum::Json(serde_json::json!({
            "error": "too many failed admin auth attempts, IP locked",
            "retry_after": retry_after_secs,
        })),
    )
        .into_response();
    if let Ok(v) = retry_after_secs.to_string().parse() {
        resp.headers_mut().insert("retry-after", v);
    }
    resp
}

// ---------------------------------------------------------------------------
// admin 失败次数限频 (防暴力穷举密码)
// ---------------------------------------------------------------------------

/// 同一 IP 在 [WINDOW_SECS] 秒内连续失败 [LOCK_THRESHOLD] 次 → 锁 [LOCK_SECS] 秒。
const ADMIN_FAIL_THRESHOLD: u32 = 5;
const ADMIN_FAIL_WINDOW_SECS: u64 = 300; // 失败计数窗口 5 分钟
const ADMIN_LOCK_SECS: u64 = 300; // 锁定 5 分钟
/// DashMap 硬上限 (修 N4): 防穷举攻击撑爆内存。
const ADMIN_FAIL_HARD_CAP: usize = 50_000;
/// 后台 GC 周期: 删除已过期 entry。
const ADMIN_FAIL_GC_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

struct AdminFailState {
    fail_count: u32,
    first_fail_at: std::time::Instant,
    lock_until: Option<std::time::Instant>,
}

static ADMIN_FAIL_TRACKER: once_cell::sync::Lazy<
    dashmap::DashMap<String, AdminFailState>,
> = once_cell::sync::Lazy::new(|| {
    let map = dashmap::DashMap::new();
    // 修 N4: 启动后台 GC 永久 task, 60s 一轮清过期 entry
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(ADMIN_FAIL_GC_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let now = std::time::Instant::now();
            ADMIN_FAIL_TRACKER.retain(|_, s| {
                // 保留: 仍在锁定期内 OR 失败窗口仍有效
                s.lock_until.map(|t| t > now).unwrap_or(false)
                    || now.duration_since(s.first_fail_at).as_secs() <= ADMIN_FAIL_WINDOW_SECS
            });
        }
    });
    map
});

/// 检查 IP 是否当前被锁; 返回 Some(剩余秒数) 表示锁定。
fn check_admin_locked(client_ip: &str) -> Option<u64> {
    let now = std::time::Instant::now();
    let entry = ADMIN_FAIL_TRACKER.get(client_ip)?;
    let state = entry.value();
    if let Some(until) = state.lock_until {
        if until > now {
            return Some(until.duration_since(now).as_secs());
        }
    }
    None
}

fn record_admin_fail(client_ip: &str) {
    let now = std::time::Instant::now();
    // 修 N4: 命中硬上限时拒绝新增 entry, 防穷举攻击 (大 IP 池打过来)
    if !ADMIN_FAIL_TRACKER.contains_key(client_ip)
        && ADMIN_FAIL_TRACKER.len() >= ADMIN_FAIL_HARD_CAP
    {
        tracing::warn!(
            "ADMIN_FAIL_TRACKER hit hard cap ({}), refusing to track new IP",
            ADMIN_FAIL_HARD_CAP
        );
        return;
    }
    let mut entry = ADMIN_FAIL_TRACKER
        .entry(client_ip.to_string())
        .or_insert_with(|| AdminFailState {
            fail_count: 0,
            first_fail_at: now,
            lock_until: None,
        });
    let s = entry.value_mut();
    // 已锁定的不再叠加, 直接维持原 lock_until
    if s.lock_until.map(|t| t > now).unwrap_or(false) {
        return;
    }
    // 失败窗口已过, 重置
    if now.duration_since(s.first_fail_at).as_secs() > ADMIN_FAIL_WINDOW_SECS {
        s.fail_count = 0;
        s.first_fail_at = now;
        s.lock_until = None;
    }
    s.fail_count += 1;
    if s.fail_count >= ADMIN_FAIL_THRESHOLD {
        s.lock_until = Some(now + std::time::Duration::from_secs(ADMIN_LOCK_SECS));
        tracing::warn!(
            client_ip = %client_ip,
            "admin auth: IP locked for {}s after {} failed attempts",
            ADMIN_LOCK_SECS,
            ADMIN_FAIL_THRESHOLD
        );
    }
}

fn record_admin_success(client_ip: &str) {
    ADMIN_FAIL_TRACKER.remove(client_ip);
}

/// 管理后台密码认证, 带 IP 失败次数限频 (防暴力穷举)
///
/// 修 N3: 必须能拿到客户端真实 IP (X-Forwarded-For / X-Real-IP), 否则直接拒。
/// 商业部署必须前置反代 (nginx/Caddy/云 LB), 反代必须注入 XFF。否则:
/// 1) 所有失败计入同一桶, 5 次失败把全部 admin 用户锁 5 分钟 (DoS)
/// 2) 单 IP 攻击者打穿密码也不会被限频
///
/// **本地开发 / 单机直连场景**: 设 env `CCBRIDGE_TRUST_DIRECT_IP=1` 跳过 XFF 检查,
/// 此时所有直连共用 `_direct_` 桶 — 仍受 5 次失败锁 5 分钟保护, 但只适合
/// 单运维场景, 多人共用 admin 不要开。
pub async fn admin_auth(password: String, req: Request, next: Next) -> Result<Response, Response> {
    let allow_direct = std::env::var("CCBRIDGE_TRUST_DIRECT_IP").unwrap_or_default() == "1";
    let client_ip = match extract_client_ip(&req) {
        Some(ip) => ip,
        None => {
            if allow_direct {
                // 本地开发模式: 用占位 IP 走正常 fail tracker, 仍受 5 次锁保护
                "_direct_".to_string()
            } else {
                tracing::warn!(
                    "admin auth: no X-Forwarded-For/X-Real-IP header — direct exposure to public is unsafe; \
                     deploy with nginx/Caddy/cloud LB in front and inject XFF. \
                     Local-dev override: export CCBRIDGE_TRUST_DIRECT_IP=1"
                );
                return Err(err_response(
                    "admin endpoint requires reverse proxy with X-Forwarded-For \
                     (deployment misconfig); for local dev set CCBRIDGE_TRUST_DIRECT_IP=1"
                ));
            }
        }
    };
    if let Some(retry) = check_admin_locked(&client_ip) {
        return Err(locked_response(retry));
    }
    let key = extract_key(&req);
    if key.is_empty() || key.as_bytes().ct_eq(password.as_bytes()).unwrap_u8() != 1 {
        record_admin_fail(&client_ip);
        return Err(err_response("invalid password"));
    }
    record_admin_success(&client_ip);
    Ok(next.run(req).await)
}

/// 网关令牌认证（从数据库查询）
/// 验证通过后将 ApiToken 存入请求扩展供后续 handler 使用。
pub async fn gateway_auth(
    token_store: Arc<TokenStore>,
    mut req: Request,
    next: Next,
) -> Result<Response, Response> {
    let key = extract_key(&req);
    if key.is_empty() {
        return Err(err_response("missing api key"));
    }

    let token = token_store
        .get_by_token(&key)
        .await
        .map_err(|_| err_response("authentication failed"))?
        .ok_or_else(|| err_response("invalid api key"))?;

    req.extensions_mut().insert(token);
    Ok(next.run(req).await)
}
