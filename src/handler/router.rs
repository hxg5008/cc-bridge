use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::{DateTime, TimeZone, Utc};
use rust_embed::Embed;
use serde::Deserialize;
use std::sync::Arc;

use crate::config::Config;
use crate::error::AppError;
use crate::middleware::auth::{admin_auth, extract_key};
use crate::model::account::{Account, AccountAuthType, AccountStatus};
use crate::model::api_token::{self, ApiToken};
use crate::service::account::AccountService;
use crate::service::gateway::GatewayService;
use crate::service::oauth::TokenTester;
use crate::service::oauth_flow::OAuthFlowService;
use crate::service::openai_oauth::OpenAIOAuthService;
use crate::service::telemetry::TelemetryService;
use crate::store::cache::CacheStore;
use crate::store::token_store::TokenStore;

#[derive(Clone)]
pub struct AppState {
    pub gateway_svc: Arc<GatewayService>,
    pub account_svc: Arc<AccountService>,
    pub token_tester: Arc<TokenTester>,
    pub token_store: Arc<TokenStore>,
    pub oauth_flow_svc: Arc<OAuthFlowService>,
    pub openai_oauth_svc: Arc<OpenAIOAuthService>,
    pub telemetry_svc: Arc<TelemetryService>,
    pub admin_password: String,
    /// readyz 用 cache.ping() 验证 Redis 连通; 也可未来给其它 handler 用
    pub cache: Arc<dyn CacheStore>,
}

pub fn build_router(
    cfg: &Config,
    gateway_svc: Arc<GatewayService>,
    account_svc: Arc<AccountService>,
    token_tester: Arc<TokenTester>,
    token_store: Arc<TokenStore>,
    oauth_flow_svc: Arc<OAuthFlowService>,
    openai_oauth_svc: Arc<OpenAIOAuthService>,
    telemetry_svc: Arc<TelemetryService>,
    cache: Arc<dyn CacheStore>,
) -> Router {
    let state = AppState {
        gateway_svc,
        account_svc,
        token_tester,
        token_store,
        oauth_flow_svc,
        openai_oauth_svc,
        telemetry_svc,
        admin_password: cfg.admin.password.clone(),
        cache,
    };

    let admin_password = state.admin_password.clone();

    // 前端页面（显式注册 SPA 路由）
    let frontend_routes = Router::new()
        .route("/", get(spa_handler))
        .route("/login", get(spa_handler))
        .route("/tokens", get(spa_handler))
        .route("/cache-stats", get(spa_handler));

    // 前端静态资源
    let asset_routes = Router::new()
        .route("/assets/*rest", get(asset_handler))
        .route("/favicon.svg", get(asset_handler));

    // 健康检查端点 (公开, 无需鉴权; 容器编排 / 反代探活用)
    // /metrics 已从这里挪走 → 改挂在 admin 路由下需要密码,
    // 防止外网扫到端口的人拿走运营敏感指标 (账号 token/请求量/失败率)
    let health_routes = Router::new()
        .route("/livez", get(livez_handler))
        .route("/readyz", get(readyz_handler))
        .with_state(state.clone());

    // 管理 API（密码认证，完整路径注册）
    let admin_routes = Router::new()
        .route("/admin/accounts", get(list_accounts).post(create_account))
        .route(
            "/admin/accounts/:id",
            put(update_account).delete(delete_account),
        )
        .route("/admin/accounts/batch-delete", post(batch_delete_accounts))
        .route("/admin/accounts/:id/test", post(test_account))
        .route("/admin/accounts/:id/usage", post(refresh_usage))
        .route("/admin/accounts/refresh-all-usage", post(refresh_all_usage))
        .route("/admin/accounts/:id/clear_limit", post(clear_limit_state))
        .route("/admin/cache-stats", get(cache_stats))
        // metrics 端点暴露每账号 token / 请求量 / cache 命中等运营敏感数据,
        // 必须鉴权; prometheus scrape 时配 admin 密码到 basic_auth 即可
        .route("/admin/metrics", get(metrics_handler))
        .route("/admin/tokens", get(list_tokens).post(create_token))
        .route(
            "/admin/tokens/:id",
            put(update_token).delete(delete_token_handler),
        )
        .route("/admin/dashboard", get(get_dashboard))
        .route(
            "/admin/oauth/generate-auth-url",
            post(oauth_generate_auth_url),
        )
        .route(
            "/admin/oauth/generate-setup-token-url",
            post(oauth_generate_setup_token_url),
        )
        .route("/admin/oauth/exchange-code", post(oauth_exchange_code))
        .route(
            "/admin/oauth/exchange-setup-token-code",
            post(oauth_exchange_setup_token_code),
        )
        .route("/admin/accounts/cookie-auth", post(oauth_cookie_auth))
        .route(
            "/admin/accounts/cookie-auth-create",
            post(oauth_cookie_auth_create),
        )
        .route(
            "/admin/accounts/cookie-auth-create/batch",
            post(oauth_cookie_auth_create_batch),
        )
        .route("/admin/accounts/openai", post(create_openai_account))
        .route(
            "/admin/accounts/openai-rt-import",
            post(openai_rt_import),
        )
        .route(
            "/admin/accounts/openai-rt-import/batch",
            post(openai_rt_import_batch),
        )
        .route(
            "/admin/accounts/:id/openai-usage/probe",
            post(openai_usage_probe),
        )
        .route(
            "/admin/openai-oauth/generate-auth-url",
            post(openai_oauth_generate_auth_url),
        )
        .route(
            "/admin/openai-oauth/exchange-code",
            post(openai_oauth_exchange_code),
        )
        .route(
            "/admin/openai-oauth/refresh-token",
            post(openai_oauth_refresh_token),
        )
        .layer(middleware::from_fn(move |req, next: Next| {
            let pwd = admin_password.clone();
            admin_auth(pwd, req, next)
        }))
        .with_state(state.clone());

    // CORS: admin API 跨域调用 (浏览器从其他域名打开 admin 面板) 需要;
    // gateway 透传路径不需要 (claude-code 是 native 客户端不走浏览器)。
    // 用宽松策略 (允许任何 origin/header), 安全性靠 admin password 保证。
    let cors = tower_http::cors::CorsLayer::new()
        .allow_origin(tower_http::cors::Any)
        .allow_methods(tower_http::cors::Any)
        .allow_headers(tower_http::cors::Any);

    // 全局 in-flight 请求 cap (修 C2): 防 OOM / runtime 打爆。
    // 计算公式: SSE 长连基线 (200/实例) + 短请求峰值缓冲 (~800) ≈ 1000;
    // 可通过环境变量 `CCBRIDGE_GLOBAL_INFLIGHT_CAP` 调整 (压测后改)。
    // 超过 cap 的请求 立刻返 503 "service overloaded", 不堆积 tokio task。
    let global_cap: usize = std::env::var("CCBRIDGE_GLOBAL_INFLIGHT_CAP")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(1000);
    tracing::info!("global in-flight cap: {} concurrent requests", global_cap);
    let global_sem = Arc::new(tokio::sync::Semaphore::new(global_cap));

    // 单请求 body 上限 (修 C2): 默认 2MB 防大 body 撑爆内存。
    // /v1/messages 实际最大约 100-200KB (system + messages 文本), 2MB 极保守。
    // SessionKey 批量导入 / OAuth 等长 body 请求由各 handler 自己用 to_bytes(10MB) 二次校验。
    let body_limit_bytes: usize = std::env::var("CCBRIDGE_BODY_LIMIT_MB")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(2)
        * 1024
        * 1024;

    // 组合路由：前端 + 管理 API + 其余全部透传网关
    Router::new()
        .merge(frontend_routes)
        .merge(asset_routes)
        .merge(health_routes)
        .merge(admin_routes)
        .fallback(gateway_fallback)
        // body size 限制 (单请求最大字节数, 防大 body 撑爆内存)
        .layer(axum::extract::DefaultBodyLimit::max(body_limit_bytes))
        // 全局并发 cap: 超 N 个 in-flight 请求立刻 503, 不让 tokio worker 堆积
        .layer(middleware::from_fn(move |req, next: Next| {
            let sem = global_sem.clone();
            global_concurrency_middleware(sem, req, next)
        }))
        // CORS (admin 跨域支持)
        .layer(cors)
        .with_state(state)
}

/// 全局并发限流 middleware (修 C2): 用 Semaphore::try_acquire 立刻判断,
/// 拿不到 permit 直接 503 + Retry-After=1s, 不阻塞 tokio worker。
async fn global_concurrency_middleware(
    sem: Arc<tokio::sync::Semaphore>,
    req: Request,
    next: Next,
) -> Response {
    let permit = match sem.try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            // 容量打满: 立刻 503, 不排队
            crate::service::metrics::METRICS.record_gateway_rejected("global_cap");
            tracing::warn!(
                "global in-flight cap reached, shedding request to {}",
                req.uri().path()
            );
            let mut resp = (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({
                    "error": "service overloaded, please retry shortly",
                    "type": "capacity_exceeded"
                })),
            )
                .into_response();
            if let Ok(v) = "1".parse() {
                resp.headers_mut().insert("retry-after", v);
            }
            return resp;
        }
    };
    let resp = next.run(req).await;
    drop(permit); // 显式释放; 实际 owned permit 在 drop 自动 release
    resp
}

// --- Handlers ---

/// 网关透传 fallback：鉴权 + 代理上游
async fn gateway_fallback(State(state): State<AppState>, req: Request) -> Response {
    let key = extract_key(&req);
    if key.is_empty() {
        return err_json(StatusCode::UNAUTHORIZED, "missing api key");
    }
    let api_token = match state.token_store.get_by_token(&key).await {
        Ok(Some(t)) => t,
        Ok(None) => return err_json(StatusCode::UNAUTHORIZED, "invalid api key"),
        Err(_) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, "authentication failed"),
    };

    // OpenAI 协议路径分发: /v1/chat/completions 与 /v1/responses 走 OpenAI 账号池
    let path = req.uri().path();
    if path == "/v1/chat/completions" {
        return openai_proxy_request(&state, req, "/v1/chat/completions", &api_token)
            .await
            .unwrap_or_else(|e| err_json(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()));
    }
    if path == "/v1/responses" || path.starts_with("/v1/responses/") {
        return openai_proxy_request(&state, req, "/v1/responses", &api_token)
            .await
            .unwrap_or_else(|e| err_json(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()));
    }

    state
        .gateway_svc
        .handle_request(req, Some(&api_token))
        .await
}

/// OpenAI /v1/* 透传 helper (供 /v1/chat/completions 和 /v1/responses 共用)
async fn openai_proxy_request(
    state: &AppState,
    req: Request,
    upstream_path: &str,
    api_token: &ApiToken,
) -> Result<Response, AppError> {
    crate::service::metrics::METRICS
        .record_gateway_request(crate::service::metrics::Platform::OpenAI);
    openai_chat_completions_inner(state, req, upstream_path, api_token).await
}

/// 构造并发送一次上游请求 (供 retry loop 复用)。
///
/// 输入:
///   - `account`: 已选好且(若 OAuth)已刷新过的账号
///   - `client_headers`: 客户端原始请求头, 用于白名单透传
///   - `body_bytes`: 客户端原始 body
///   - `body_json_orig`: 已解析 body (Null 表示非 JSON)
///   - `client_pck`: 客户端 prompt_cache_key (优先)
///   - `raw_seed`: derive_session_seed 的结果, 用于 session_id 派生
///   - `api_token`: 鉴权后的 token, id 用于 session 隔离
///   - `upstream_path`: `/v1/chat/completions` 或 `/v1/responses`
async fn build_and_send_openai(
    account: &Account,
    client_headers: &axum::http::HeaderMap,
    body_bytes: &axum::body::Bytes,
    body_json_orig: &serde_json::Value,
    client_pck: Option<&str>,
    raw_seed: &str,
    api_token: &ApiToken,
    upstream_path: &str,
) -> Result<reqwest::Response, AppError> {
    // 选 token
    let token = if !account.access_token.is_empty() {
        account.access_token.clone()
    } else {
        account.setup_token.clone()
    };
    if token.is_empty() {
        return Err(AppError::ServiceUnavailable(
            "selected account has no token".into(),
        ));
    }

    let is_oauth = account.auth_type == AccountAuthType::Oauth;
    let extra = account.extra.as_object();
    let organization_id = extra
        .and_then(|m| m.get("organization_id"))
        .and_then(|v| v.as_str());
    let chatgpt_account_id = extra
        .and_then(|m| m.get("chatgpt_account_id"))
        .and_then(|v| v.as_str());

    // Codex body transform (仅 OAuth)
    let final_body: Vec<u8> = if is_oauth && body_json_orig.is_object() {
        let mut body_json = body_json_orig.clone();
        let _result = crate::service::codex_transform::apply_codex_oauth_transform(
            &mut body_json,
            true,
        );

        if client_pck.is_none() {
            let model = body_json
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !raw_seed.is_empty()
                && crate::service::codex_session::should_auto_inject_prompt_cache_key(&model)
            {
                if let Some(map) = body_json.as_object_mut() {
                    map.insert(
                        "prompt_cache_key".into(),
                        serde_json::Value::String(raw_seed.to_string()),
                    );
                }
            }
        }

        serde_json::to_vec(&body_json).unwrap_or_else(|_| body_bytes.to_vec())
    } else {
        body_bytes.to_vec()
    };

    // URL 决策
    let (upstream_url, host_override, ua_default) = if is_oauth {
        (
            "https://chatgpt.com/backend-api/codex/responses".to_string(),
            Some("chatgpt.com"),
            "codex_cli_rs/0.104.0",
        )
    } else {
        let base = extra
            .and_then(|m| m.get("base_url"))
            .and_then(|v| v.as_str())
            .unwrap_or("https://api.openai.com");
        (
            format!("{}{}", base.trim_end_matches('/'), upstream_path),
            None,
            "OpenAI/Python 1.40.0",
        )
    };

    // 构造上游请求
    let client = crate::tlsfp::make_request_client(&account.proxy_url);
    let mut up_req = client
        .post(&upstream_url)
        .header("Authorization", format!("Bearer {}", token))
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream")
        .body(final_body);

    let user_agent_extra = extra
        .and_then(|m| m.get("user_agent"))
        .and_then(|v| v.as_str());
    let ua: &str = if is_oauth {
        "codex_cli_rs/0.104.0"
    } else {
        user_agent_extra.unwrap_or(ua_default)
    };
    up_req = up_req.header("User-Agent", ua);

    if is_oauth {
        if let Some(host) = host_override {
            up_req = up_req.header("Host", host);
        }
        up_req = up_req
            .header("OpenAI-Beta", "responses=experimental")
            .header("Originator", "codex_cli_rs")
            .header("Version", "0.104.0");

        if let Some(cid) = chatgpt_account_id {
            up_req = up_req.header("chatgpt-account-id", cid);
        }

        if !raw_seed.is_empty() {
            let isolated =
                crate::service::codex_session::isolate_session_id(api_token.id, raw_seed);
            let session_uuid =
                crate::service::codex_session::generate_session_uuid(&isolated);
            up_req = up_req
                .header("session_id", session_uuid.clone())
                .header("conversation_id", session_uuid);
        }
    } else if let Some(oid) = organization_id {
        up_req = up_req.header("OpenAI-Organization", oid);
    }

    // 透传客户端白名单头
    for (name, value) in client_headers.iter() {
        let n = name.as_str().to_lowercase();
        if !matches!(
            n.as_str(),
            "openai-beta" | "openai-organization" | "x-request-id" | "x-stainless-lang"
        ) {
            continue;
        }
        if is_oauth && (n == "openai-beta" || n == "openai-organization") {
            continue;
        }
        up_req = up_req.header(name.as_str(), value.as_bytes());
    }

    up_req
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("upstream request: {}", e)))
}

/// OpenAI 转发核心逻辑。
///
/// 路由策略:
///   - OAuth 账号 (Plus/Pro/Team OAuth token) → chatgpt.com/backend-api/codex/responses
///     带 Codex CLI 专用 header: chatgpt-account-id, Originator, OpenAI-Beta, Version
///     并对 body 做完整 codex transform (store=false / system 提取 / 不支持参数清理 / 等)
///   - API Key / Codex Token / Setup Token → api.openai.com (extra.base_url 可覆盖)
///
/// 防风控关键点 (sub2api 同款):
///   - User-Agent 强制 codex_cli_rs/0.104.0, Originator 强制 codex_cli_rs (硬编码不可关)
///   - instructions 为空时填充嵌入的官方 Codex CLI 模板
///   - session_id / conversation_id 用 prompt_cache_key (或 body 内容种子) 派生 UUID,
///     api_token id 做隔离, 防跨 token prompt cache 串扰
///   - 粘性会话: 同一 prompt_cache_key 粘到同一 OpenAI 账号 (24h TTL)
async fn openai_chat_completions_inner(
    state: &AppState,
    req: Request,
    upstream_path: &str,
    api_token: &ApiToken,
) -> Result<Response, AppError> {
    use axum::body::{to_bytes, Body};
    use axum::http::HeaderValue;

    // ---- 1) 读 client body ----
    let (parts, body) = req.into_parts();
    let body_bytes = to_bytes(body, 50 * 1024 * 1024)
        .await
        .map_err(|e| AppError::BadRequest(format!("read body: {}", e)))?;

    // 试解析为 JSON, 失败就保持 raw 透传 (兼容非 JSON 客户端)
    let body_json_orig: serde_json::Value =
        serde_json::from_slice(&body_bytes).unwrap_or(serde_json::Value::Null);

    // ---- 2) 派生稳定 session seed (用于粘性 + session_id 头) ----
    let client_pck = body_json_orig
        .get("prompt_cache_key")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let raw_seed = crate::service::codex_session::derive_session_seed(
        &body_json_orig,
        client_pck.as_deref(),
    );
    // 修 N3 同款 (Anthropic 路径已修, OpenAI 路径漏): 加 api_token.id 命名空间,
    // 防多租户串号 — 两个不同 api_token 用相同 prompt_cache_key 时不会
    // 命中同一 sticky 账号。
    let namespaced_seed = format!("tk{}|{}", api_token.id, raw_seed);
    let session_hash =
        crate::service::codex_session::sticky_session_cache_key(&namespaced_seed);

    let allowed_ids = api_token.allowed_account_ids();
    let mut exclude_ids = api_token.blocked_account_ids();

    // ---- 3-11) 选号 + 刷 + 构造 + 发送, 5xx 时切账号重试一次 ----
    const MAX_ATTEMPTS: u32 = 2;
    let mut attempt: u32 = 0;
    let (account, resp, status, headers) = loop {
        attempt += 1;

        // 选 OpenAI 账号 (粘性 + 黑白名单 + 5xx exclude)
        let mut account = state
            .account_svc
            .select_openai_account(&session_hash, &exclude_ids, &allowed_ids)
            .await?;

        // OAuth access_token 自动刷新 (带全局锁防 thundering herd)
        if account.auth_type == AccountAuthType::Oauth {
            match state
                .account_svc
                .resolve_openai_access_token(&account, &state.openai_oauth_svc)
                .await
            {
                Ok(refreshed) => {
                    account = refreshed;
                    crate::service::metrics::METRICS
                        .record_oauth_refresh(crate::service::metrics::Platform::OpenAI, true);
                }
                Err(e) => {
                    crate::service::metrics::METRICS
                        .record_oauth_refresh(crate::service::metrics::Platform::OpenAI, false);
                    tracing::warn!(
                        "openai resolve_access_token failed for account {}: {}",
                        account.id,
                        e
                    );
                }
            }
        }

        let resp = build_and_send_openai(
            &account,
            &parts.headers,
            &body_bytes,
            &body_json_orig,
            client_pck.as_deref(),
            &raw_seed,
            api_token,
            upstream_path,
        )
        .await?;

        let status = resp.status();
        let headers_clone = resp.headers().clone();

        if status.as_u16() >= 400 {
            crate::service::metrics::METRICS.record_gateway_error(
                crate::service::metrics::Platform::OpenAI,
                status.as_u16(),
            );
        }

        // 修 N10: OpenAI 路径 401/403 凭证失效 → 主动 disable_account, 让 dashboard
        // 看见挂掉的号; 不再让 selector 一直选这个号死循环 + cooldown 60s 然后又选。
        // (4xx 中 451 = legal blocked 也归 disable; 429 是限流不是失效, 走另一分支)
        if matches!(status.as_u16(), 401 | 403 | 451) {
            tracing::warn!(
                "openai account {} returned {} → disabling account",
                account.id,
                status.as_u16()
            );
            let svc = state.account_svc.clone();
            let acc_id = account.id;
            let reason = format!("openai upstream {} (auth/legal)", status.as_u16());
            tokio::spawn(async move {
                let _ = svc
                    .disable_account(
                        acc_id,
                        crate::model::account::AccountStatus::Disabled,
                        &reason,
                        None,
                    )
                    .await;
            });
        }

        // 5xx 失败转移: 标短期冷却 + 把当前账号加 exclude, 再选一次
        let is_5xx = (500..600).contains(&status.as_u16());
        if is_5xx && attempt < MAX_ATTEMPTS {
            tracing::warn!(
                "openai upstream 5xx ({}) on account {}, failing over",
                status.as_u16(),
                account.id
            );
            crate::service::metrics::METRICS.record_failover();

            // 不写 codex_429_until (那是 429 专属); 用 60s cooldown 短期回避
            let svc = state.account_svc.clone();
            let acc_id = account.id;
            tokio::spawn(async move {
                let until = crate::service::openai_limit::compute_429_cooldown_until(None);
                let _ = persist_codex_429_until(svc, acc_id, until).await;
            });

            exclude_ids.push(account.id);
            // resp / body 不读, 直接 drop
            drop(resp);
            continue;
        }

        break (account, resp, status, headers_clone);
    };

    let is_oauth = account.auth_type == AccountAuthType::Oauth;

    // ---- 12) 异步落用量到 account.extra ----
    if is_oauth {
        if let Some(usage_update) = parse_codex_rate_limit_headers(&headers) {
            let svc = state.account_svc.clone();
            let acc_id = account.id;
            tokio::spawn(async move {
                let _ = persist_codex_usage(svc, acc_id, usage_update).await;
            });
        }

        // 429 → 写 codex_429_until 触发账号短期冷却 (默认 60s, Retry-After 优先)
        if status.as_u16() == 429 {
            let retry_after = headers
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let svc = state.account_svc.clone();
            let acc_id = account.id;
            tokio::spawn(async move {
                let until = crate::service::openai_limit::compute_429_cooldown_until(
                    retry_after.as_deref(),
                );
                if let Err(e) = persist_codex_429_until(svc, acc_id, until).await {
                    tracing::warn!(
                        "persist codex_429_until failed for account {}: {}",
                        acc_id,
                        e
                    );
                } else {
                    tracing::warn!(
                        "openai account {} 429 → cooldown until {}",
                        acc_id,
                        until
                    );
                }
            });
        }
    }

    // ---- 13) 流式响应透传 ----
    let mut builder = Response::builder().status(
        StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
    );
    for (name, value) in headers.iter() {
        let lname = name.as_str().to_lowercase();
        if matches!(
            lname.as_str(),
            "transfer-encoding" | "connection" | "content-length"
        ) {
            continue;
        }
        builder = builder.header(
            name.as_str(),
            HeaderValue::from_bytes(value.as_bytes()).unwrap_or(HeaderValue::from_static("")),
        );
    }
    let response = builder
        .body(Body::from_stream(resp.bytes_stream()))
        .unwrap_or_else(|_| err_json(StatusCode::INTERNAL_SERVER_ERROR, "build response failed"));
    Ok(response)
}

/// 统一 JSON 错误响应
fn err_json(status: StatusCode, msg: &str) -> Response {
    (status, Json(serde_json::json!({"error": msg}))).into_response()
}

// --- Account Handlers ---

#[derive(Deserialize)]
struct PageQuery {
    page: Option<i64>,
    page_size: Option<i64>,
}

async fn list_accounts(
    State(state): State<AppState>,
    Query(query): Query<PageQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let page = query.page.unwrap_or(1).max(1);
    let page_size = query.page_size.unwrap_or(12).clamp(1, 100);
    let (accounts, total) = state
        .account_svc
        .list_accounts_paged(page, page_size)
        .await?;
    let total_pages = (total + page_size - 1) / page_size;

    // 为每个账号附加遥测会话过期时间 + 内存限流倒计时 + 分类
    let mut data: Vec<serde_json::Value> = Vec::with_capacity(accounts.len());
    for a in &accounts {
        let mut obj = serde_json::to_value(a).unwrap_or_default();
        if let Some(expires) = state.telemetry_svc.get_session_expires_at(a.id).await {
            obj["telemetry_expires_at"] = serde_json::json!(expires.to_rfc3339());
        }
        obj["current_concurrency"] =
            serde_json::json!(state.account_svc.peek_concurrency(a.id).await);
        // 内存里仍有效的短期 ban 截止时间（dashboard 上显示倒计时 + "清除限流"按钮提示用）
        if let Some(until) = state.account_svc.peek_rate_limited_until(a.id) {
            obj["rate_limited_until_runtime"] = serde_json::json!(until.to_rfc3339());
        }
        // 5 类用户视角分类（前端徽章 + 筛选用）
        let cat = state.account_svc.categorize(a);
        obj["category"] = serde_json::to_value(cat.category).unwrap_or_default();
        if !cat.reason.is_empty() {
            obj["category_reason"] = serde_json::json!(cat.reason);
        }
        if let Some(t) = cat.recovers_at {
            obj["category_recovers_at"] = serde_json::json!(t.to_rfc3339());
        }
        data.push(obj);
    }

    Ok(Json(serde_json::json!({
        "data": data,
        "total": total,
        "page": page,
        "page_size": page_size,
        "total_pages": total_pages,
    })))
}

#[derive(Deserialize)]
struct CreateAccountRequest {
    name: Option<String>,
    email: String,
    token: Option<String>,
    setup_token: Option<String>,
    auth_type: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_at: Option<ClientDateTime>,
    proxy_url: Option<String>,
    billing_mode: Option<String>,
    account_uuid: Option<String>,
    organization_uuid: Option<String>,
    subscription_type: Option<String>,
    concurrency: Option<i32>,
    priority: Option<i32>,
    auto_telemetry: Option<bool>,
    experimental_reveal_thinking: Option<bool>,
    enable_cache_ttl_1h_injection: Option<bool>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ClientDateTime {
    Millis(i64),
    Text(String),
}

async fn create_account(
    State(state): State<AppState>,
    Json(req): Json<CreateAccountRequest>,
) -> Result<(StatusCode, Json<Account>), AppError> {
    if req.email.is_empty() {
        return Err(AppError::BadRequest("email is required".into()));
    }
    // 防御性校验：rewrite 模式破坏缓存命中（cch_hash 注入到 system 块导致前缀漂移）
    if let Some(ref bm) = req.billing_mode {
        if bm == "rewrite" && std::env::var("CCBRIDGE_ALLOW_REWRITE").is_err() {
            return Err(AppError::BadRequest(
                "billing_mode='rewrite' is disabled because it breaks Anthropic prompt cache hit rate (cch_hash drifts system prefix). \
                 Set env CCBRIDGE_ALLOW_REWRITE=1 if Anthropic anti-fingerprint requires it."
                    .into(),
            ));
        }
    }
    let auth_type = req.auth_type.unwrap_or_else(|| "setup_token".into()).into();
    let setup_token = req.setup_token.or(req.token).unwrap_or_default();
    let mut account = Account {
        id: 0,
        name: req.name.unwrap_or_default(),
        email: req.email,
        status: AccountStatus::Active,
        auth_type,
        setup_token,
        access_token: req.access_token.unwrap_or_default(),
        refresh_token: req.refresh_token.unwrap_or_default(),
        expires_at: req.expires_at.as_ref().and_then(client_datetime_to_utc),
        oauth_refreshed_at: None,
        auth_error: String::new(),
        proxy_url: req.proxy_url.unwrap_or_default(),
        device_id: String::new(),
        canonical_env: serde_json::json!({}),
        canonical_prompt: serde_json::json!({}),
        canonical_process: serde_json::json!({}),
        billing_mode: req.billing_mode.unwrap_or_else(|| "strip".into()).into(),
        account_uuid: req.account_uuid,
        organization_uuid: req.organization_uuid,
        subscription_type: req.subscription_type,
        concurrency: req.concurrency.unwrap_or(3),
        priority: req.priority.unwrap_or(50),
        rate_limited_at: None,
        rate_limit_reset_at: None,
        disable_reason: String::new(),
        auto_telemetry: req.auto_telemetry.unwrap_or(false),
        telemetry_count: 0,
        experimental_reveal_thinking: req.experimental_reveal_thinking.unwrap_or(false),
        enable_cache_ttl_1h_injection: req.enable_cache_ttl_1h_injection.unwrap_or(false),
        session_key: String::new(),
        usage_data: serde_json::json!({}),
        usage_fetched_at: None,
        platform: "claude".into(),
        extra: serde_json::json!({}),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    state.account_svc.create_account(&mut account).await?;
    Ok((StatusCode::CREATED, Json(account)))
}

async fn update_account(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(updates): Json<serde_json::Value>,
) -> Result<Json<Account>, AppError> {
    let mut existing = state.account_svc.get_account(id).await?;

    if let Some(name) = updates.get("name").and_then(|v| v.as_str()) {
        if !name.is_empty() {
            existing.name = name.to_string();
        }
    }
    if let Some(email) = updates.get("email").and_then(|v| v.as_str()) {
        if !email.is_empty() {
            existing.email = email.to_string();
        }
    }
    if let Some(auth_type) = updates.get("auth_type").and_then(|v| v.as_str()) {
        existing.auth_type = auth_type.to_string().into();
        match existing.auth_type {
            AccountAuthType::SetupToken => {
                existing.access_token.clear();
                existing.refresh_token.clear();
                existing.expires_at = None;
                existing.oauth_refreshed_at = None;
                existing.auth_error.clear();
            }
            AccountAuthType::Oauth => {
                existing.setup_token.clear();
            }
        }
    }
    if let Some(token) = updates.get("token").and_then(|v| v.as_str()) {
        existing.setup_token = token.to_string();
    }
    if let Some(setup_token) = updates.get("setup_token").and_then(|v| v.as_str()) {
        existing.setup_token = setup_token.to_string();
    }
    if let Some(access_token) = updates.get("access_token").and_then(|v| v.as_str()) {
        existing.access_token = access_token.to_string();
    }
    if let Some(refresh_token) = updates.get("refresh_token").and_then(|v| v.as_str()) {
        existing.refresh_token = refresh_token.to_string();
    }
    if updates.get("expires_at").is_some() {
        existing.expires_at = updates.get("expires_at").and_then(client_datetime_value_to_utc);
    }
    if let Some(proxy_url) = updates.get("proxy_url").and_then(|v| v.as_str()) {
        existing.proxy_url = proxy_url.to_string();
    }
    if let Some(concurrency) = updates.get("concurrency").and_then(|v| v.as_i64()) {
        if concurrency > 0 {
            existing.concurrency = concurrency as i32;
        }
    }
    if let Some(priority) = updates.get("priority").and_then(|v| v.as_i64()) {
        if priority > 0 {
            existing.priority = priority as i32;
        }
    }
    if let Some(status) = updates.get("status").and_then(|v| v.as_str()) {
        if !status.is_empty() {
            if status == "active" {
                state.account_svc.enable_account(id).await?;
                existing = state.account_svc.get_account(id).await?;
                return Ok(Json(existing));
            } else if status == "disabled" {
                state
                    .account_svc
                    .disable_account(id, AccountStatus::Disabled, "手动停用", None)
                    .await?;
                existing = state.account_svc.get_account(id).await?;
                return Ok(Json(existing));
            } else {
                existing.status = status.to_string().into();
            }
        }
    }
    if let Some(billing_mode) = updates.get("billing_mode").and_then(|v| v.as_str()) {
        if !billing_mode.is_empty() {
            // 防御性校验：rewrite 模式破坏缓存命中
            if billing_mode == "rewrite" && std::env::var("CCBRIDGE_ALLOW_REWRITE").is_err() {
                return Err(AppError::BadRequest(
                    "billing_mode='rewrite' is disabled because it breaks Anthropic prompt cache hit rate (cch_hash drifts system prefix). \
                     Set env CCBRIDGE_ALLOW_REWRITE=1 if Anthropic anti-fingerprint requires it."
                        .into(),
                ));
            }
            existing.billing_mode = billing_mode.to_string().into();
        }
    }
    if updates.get("account_uuid").is_some() {
        existing.account_uuid = updates
            .get("account_uuid")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
    }
    if updates.get("organization_uuid").is_some() {
        existing.organization_uuid = updates
            .get("organization_uuid")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
    }
    if updates.get("subscription_type").is_some() {
        existing.subscription_type = updates
            .get("subscription_type")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
    }
    if let Some(auto_telemetry) = updates.get("auto_telemetry").and_then(|v| v.as_bool()) {
        existing.auto_telemetry = auto_telemetry;
    }
    if let Some(reveal) = updates
        .get("experimental_reveal_thinking")
        .and_then(|v| v.as_bool())
    {
        existing.experimental_reveal_thinking = reveal;
    }
    if let Some(inject) = updates
        .get("enable_cache_ttl_1h_injection")
        .and_then(|v| v.as_bool())
    {
        existing.enable_cache_ttl_1h_injection = inject;
    }
    if let Some(sk) = updates.get("session_key").and_then(|v| v.as_str()) {
        // session_key 可以更新（包括清空）；明文存储，运维需保护 DB 访问权限
        existing.session_key = sk.to_string();
    }

    // OpenAI 平台专用字段 partial merge 到 extra (chatgpt_account_id / organization_id / base_url 等)
    if let Some(extra_patch) = updates.get("extra").and_then(|v| v.as_object()) {
        let mut current = match existing.extra.as_object() {
            Some(o) => o.clone(),
            None => serde_json::Map::new(),
        };
        for (k, v) in extra_patch.iter() {
            // null 或空字符串 → 删除该字段;否则覆盖
            let is_empty_string = matches!(v, serde_json::Value::String(s) if s.is_empty());
            if v.is_null() || is_empty_string {
                current.remove(k);
            } else {
                current.insert(k.clone(), v.clone());
            }
        }
        existing.extra = serde_json::Value::Object(current);
    }

    state.account_svc.update_account(&existing).await?;
    Ok(Json(existing))
}

async fn delete_account(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, AppError> {
    state.account_svc.delete_account(id).await?;
    Ok(Json(serde_json::json!({"status": "deleted"})))
}

#[derive(Deserialize)]
struct BatchDeleteRequest {
    ids: Vec<i64>,
}

#[derive(serde::Serialize)]
struct BatchDeleteFailure {
    id: i64,
    error: String,
}

/// POST /admin/accounts/batch-delete — 批量删除账号
///
/// 单条失败不阻塞其他, 返回成功 / 失败计数 + 失败明细 (最多 5 条)。
async fn batch_delete_accounts(
    State(state): State<AppState>,
    Json(req): Json<BatchDeleteRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    if req.ids.is_empty() {
        return Err(AppError::BadRequest("ids 为空".into()));
    }
    let mut deleted = 0i32;
    let mut failed = 0i32;
    let mut errors: Vec<BatchDeleteFailure> = Vec::new();

    for id in &req.ids {
        match state.account_svc.delete_account(*id).await {
            Ok(()) => deleted += 1,
            Err(e) => {
                failed += 1;
                if errors.len() < 5 {
                    errors.push(BatchDeleteFailure {
                        id: *id,
                        error: e.to_string(),
                    });
                }
            }
        }
    }
    Ok(Json(serde_json::json!({
        "status": "ok",
        "deleted": deleted,
        "failed": failed,
        "errors": errors,
    })))
}

async fn test_account(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, AppError> {
    let account = state.account_svc.get_account(id).await?;
    // OpenAI 账号: 用 OAuth refresh_token 调一次 auth.openai.com 验证
    if account.platform == "openai" {
        if account.auth_type == AccountAuthType::Oauth && !account.refresh_token.is_empty() {
            return Ok(Json(
                match state
                    .openai_oauth_svc
                    .refresh_token(&account.refresh_token, &account.proxy_url)
                    .await
                {
                    Ok(refreshed) => {
                        // 持久化: 把新 token + expires_at 写回, 顺便清掉 auth_error
                        let mut updated = account.clone();
                        updated.access_token = refreshed.access_token.clone();
                        if !refreshed.refresh_token.is_empty() {
                            updated.refresh_token = refreshed.refresh_token.clone();
                        }
                        updated.expires_at =
                            Utc.timestamp_opt(refreshed.expires_at, 0).single();
                        updated.auth_error = String::new();
                        // 把 chatgpt_account_id / organization_id 也补到 extra (refresh 时可能拿到新的)
                        if !refreshed.chatgpt_account_id.is_empty()
                            || !refreshed.organization_id.is_empty()
                            || !refreshed.plan_type.is_empty()
                            || !refreshed.email.is_empty()
                            || !refreshed.subscription_expires_at.is_empty()
                            || !refreshed.privacy_mode.is_empty()
                        {
                            let mut extra = match updated.extra.as_object() {
                                Some(o) => o.clone(),
                                None => serde_json::Map::new(),
                            };
                            if !refreshed.chatgpt_account_id.is_empty() {
                                extra.insert(
                                    "chatgpt_account_id".into(),
                                    serde_json::json!(refreshed.chatgpt_account_id),
                                );
                            }
                            if !refreshed.organization_id.is_empty() {
                                extra.insert(
                                    "organization_id".into(),
                                    serde_json::json!(refreshed.organization_id),
                                );
                            }
                            if !refreshed.plan_type.is_empty() {
                                extra.insert(
                                    "plan_type".into(),
                                    serde_json::json!(refreshed.plan_type),
                                );
                            }
                            if !refreshed.subscription_expires_at.is_empty() {
                                extra.insert(
                                    "subscription_expires_at".into(),
                                    serde_json::json!(refreshed.subscription_expires_at),
                                );
                            }
                            if !refreshed.privacy_mode.is_empty() {
                                extra.insert(
                                    "privacy_mode".into(),
                                    serde_json::json!(refreshed.privacy_mode),
                                );
                            }
                            updated.extra = serde_json::Value::Object(extra);
                        }
                        let _ = state.account_svc.update_account(&updated).await;
                        serde_json::json!({"status": "ok"})
                    }
                    Err(e) => serde_json::json!({"status": "error", "message": e.to_string()}),
                },
            ));
        }
        // API Key / Codex Token 模式: 简单调一次 /v1/models 验证 token 活性
        let token = if !account.access_token.is_empty() {
            account.access_token.clone()
        } else {
            account.setup_token.clone()
        };
        if token.is_empty() {
            return Ok(Json(
                serde_json::json!({"status": "error", "message": "no token to test"}),
            ));
        }
        let base = account
            .extra
            .get("base_url")
            .and_then(|v| v.as_str())
            .unwrap_or("https://api.openai.com");
        let url = format!("{}/v1/models", base.trim_end_matches('/'));
        let client = crate::tlsfp::make_request_client(&account.proxy_url);
        return Ok(Json(
            match client
                .get(&url)
                .header("Authorization", format!("Bearer {}", token))
                // identity 防止上游回 gzip (reqwest 没启 gzip feature, .text() 会读到压缩字节乱码)
                .header("accept-encoding", "identity")
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => serde_json::json!({"status": "ok"}),
                Ok(r) => {
                    let s = r.status();
                    let t = r.text().await.unwrap_or_default();
                    serde_json::json!({"status": "error", "message": format!("{} {}", s, t)})
                }
                Err(e) => serde_json::json!({"status": "error", "message": e.to_string()}),
            },
        ));
    }

    // Claude 账号: 走原 TokenTester
    let token = match state.account_svc.resolve_upstream_token(id).await {
        Ok(token) => token,
        Err(e) => {
            return Ok(Json(
                serde_json::json!({"status": "error", "message": e.to_string()}),
            ));
        }
    };
    match state
        .token_tester
        .test_token(&token, &account.proxy_url, &account.canonical_env)
        .await
    {
        Ok(()) => {
            // test 成功 = 上游真的能用 = LimitStore 里如果还残留 rejected 就是假阳性，自动清。
            // 这是给运维一个"人工拨"的逃生口：怀疑账号被卡住时，点测试按钮即可恢复。
            let cleared = state.account_svc.clear_limit_runtime_flags(id);
            Ok(Json(serde_json::json!({
                "status": "ok",
                "auto_cleared_limit": cleared,
            })))
        }
        Err(e) => Ok(Json(
            serde_json::json!({"status": "error", "message": e.to_string()}),
        )),
    }
}

async fn refresh_usage(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, AppError> {
    match state.account_svc.refresh_usage(id).await {
        Ok(usage) => Ok(Json(serde_json::json!({"status": "ok", "usage": usage}))),
        Err(e) => {
            // BadRequest 通常是已经做过文案处理（如 SetupToken 提示），直接返回原消息；
            // TooManyRequests 给出通用的限频文案；其他错误走默认 display。
            let message = match &e {
                AppError::BadRequest(msg) => msg.clone(),
                AppError::TooManyRequests(_) => "用量查询接口超限，请一分钟后再试".to_string(),
                _ => e.to_string(),
            };
            Ok(Json(
                serde_json::json!({"status": "error", "message": message}),
            ))
        }
    }
}

/// POST /admin/accounts/refresh-all-usage — 批量刷新所有 OAuth 账号的用量。
///
/// 设计:
/// - 仅遍历 `auth_type=oauth` 的活跃账号 (SetupToken 没有 /api/oauth/usage 端点)
/// - 单账号走 `refresh_usage()` 内部 60s DB 缓存 + 60s 429 cooldown,
///   所以频繁调用此 endpoint 不会真打上游 (Anthropic /api/oauth/usage 限频很严)
/// - 失败的账号不阻塞其他账号 (单条 catch + 计数返回)
/// - 串行执行避免对上游瞬时打太多并发
async fn refresh_all_usage(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, AppError> {
    let accounts = state.account_svc.list_accounts().await?;
    let mut ok_count = 0i32;
    let mut skipped = 0i32;
    let mut failed = 0i32;
    let mut errors: Vec<serde_json::Value> = Vec::new();

    for a in &accounts {
        if a.status != AccountStatus::Active {
            skipped += 1;
            continue;
        }
        if a.auth_type != AccountAuthType::Oauth {
            skipped += 1;
            continue;
        }
        match state.account_svc.refresh_usage(a.id).await {
            Ok(_) => ok_count += 1,
            Err(e) => {
                failed += 1;
                if errors.len() < 5 {
                    errors.push(serde_json::json!({
                        "id": a.id,
                        "email": a.email,
                        "error": e.to_string(),
                    }));
                }
            }
        }
    }

    Ok(Json(serde_json::json!({
        "status": "ok",
        "ok": ok_count,
        "skipped": skipped,
        "failed": failed,
        "errors": errors,
    })))
}

/// 手动清除指定账号的内存软限流标记（rate_limited_until / status=Rejected）。
///
/// 触发场景：admin UI 显示账号 5h/7d 用量已重置但调度器仍持续过滤该账号——
/// 这通常是 `absorb_headers` 写入的旧标记没有机会被新请求刷新（因为本地挡了所以
/// 没请求发出去 → 死锁）。`refresh_usage` 自动清理已经覆盖大部分场景，此 endpoint
/// 作为最后的人工逃生口。
async fn clear_limit_state(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, AppError> {
    let cleared = state.account_svc.clear_limit_runtime_flags(id);
    Ok(Json(serde_json::json!({
        "status": "ok",
        "cleared": cleared,
    })))
}

/// GET /admin/cache-stats — 总体缓存命中率统计 + 按账号明细。
/// 数据来自启动至今的 atomic counter（重启清零，但 Prometheus 风格 counter 即可）。
/// 用于前端首页 Dashboard 展示，不需要任何额外存储或聚合。
async fn cache_stats(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, AppError> {
    use std::sync::atomic::Ordering;
    let m = &crate::service::metrics::METRICS;
    let input = m.anthropic_input_tokens_total.load(Ordering::Relaxed);
    let read = m.anthropic_cache_read_tokens_total.load(Ordering::Relaxed);
    let c5m = m.anthropic_cache_creation_5m_tokens_total.load(Ordering::Relaxed);
    let c1h = m.anthropic_cache_creation_1h_tokens_total.load(Ordering::Relaxed);
    let sniffed = m.anthropic_usage_sniffed_total.load(Ordering::Relaxed);

    let total = input + read + c5m + c1h;
    let hit_rate_pct = if total > 0 {
        (read as f64) / (total as f64) * 100.0
    } else {
        0.0
    };
    let one_hour_share_pct = if c5m + c1h > 0 {
        (c1h as f64) / ((c5m + c1h) as f64) * 100.0
    } else {
        0.0
    };
    // 实际加权 vs 假设全 1.0 倍率消耗
    let weighted =
        input as f64 + (c5m as f64) * 1.25 + (c1h as f64) * 2.0 + (read as f64) * 0.1;
    let saved_pct = if total > 0 {
        (1.0 - weighted / (total as f64)) * 100.0
    } else {
        0.0
    };

    let preserved = m.sticky_preserved_total.load(Ordering::Relaxed);
    let evicted = m.sticky_evicted_total.load(Ordering::Relaxed);
    let recovery_ok = m.oauth_recovery_session_key_success.load(Ordering::Relaxed);
    let recovery_fail = m.oauth_recovery_session_key_failure.load(Ordering::Relaxed);

    // 按账号明细：捞出所有 per-account snapshot + 拼接 email（用 admin API 已有的 list_paged 不够，直接查 DB）
    let mut per_account = m.snapshot_per_account_cache();
    // 按命中 token 量降序排列
    per_account.sort_by_key(|s| std::cmp::Reverse(s.cache_read_tokens));

    // 拼接 account email（从 schedulable cache 或 DB 找）
    let accounts_meta = state.account_svc.list_accounts().await.unwrap_or_default();
    let email_map: std::collections::HashMap<i64, String> = accounts_meta
        .iter()
        .map(|a| (a.id, a.email.clone()))
        .collect();
    let per_account_with_email: Vec<serde_json::Value> = per_account
        .into_iter()
        .map(|s| {
            serde_json::json!({
                "account_id": s.account_id,
                "account_email": email_map.get(&s.account_id).cloned().unwrap_or_default(),
                "input_tokens": s.input_tokens,
                "cache_read_tokens": s.cache_read_tokens,
                "cache_creation_5m_tokens": s.cache_creation_5m_tokens,
                "cache_creation_1h_tokens": s.cache_creation_1h_tokens,
                "sniffed_requests": s.sniffed_requests,
                "total_tokens": s.total_tokens,
                "hit_rate_pct": s.hit_rate_pct,
                "one_hour_share_pct": s.one_hour_share_pct,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "sniffed_requests": sniffed,
        "input_tokens": input,
        "cache_read_tokens": read,
        "cache_creation_5m_tokens": c5m,
        "cache_creation_1h_tokens": c1h,
        "total_tokens": total,
        "hit_rate_pct": hit_rate_pct,
        "one_hour_share_pct": one_hour_share_pct,
        "saved_pct": saved_pct,
        "sticky_preserved_total": preserved,
        "sticky_evicted_total": evicted,
        "oauth_recovery_success": recovery_ok,
        "oauth_recovery_failure": recovery_fail,
        "per_account": per_account_with_email,
    })))
}

// --- Token Handlers ---

async fn list_tokens(
    State(state): State<AppState>,
    Query(query): Query<PageQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let page = query.page.unwrap_or(1).max(1);
    let page_size = query.page_size.unwrap_or(20).clamp(1, 100);
    let total = state.token_store.count().await?;
    let tokens = state.token_store.list_paged(page, page_size).await?;
    let total_pages = (total + page_size - 1) / page_size;
    Ok(Json(serde_json::json!({
        "data": tokens,
        "total": total,
        "page": page,
        "page_size": page_size,
        "total_pages": total_pages,
    })))
}

#[derive(Deserialize)]
struct CreateTokenRequest {
    name: Option<String>,
    allowed_accounts: Option<String>,
    blocked_accounts: Option<String>,
}

async fn create_token(
    State(state): State<AppState>,
    Json(req): Json<CreateTokenRequest>,
) -> Result<(StatusCode, Json<ApiToken>), AppError> {
    let mut token = ApiToken {
        id: 0,
        name: req.name.unwrap_or_default(),
        token: api_token::generate_token(),
        allowed_accounts: req.allowed_accounts.unwrap_or_default(),
        blocked_accounts: req.blocked_accounts.unwrap_or_default(),
        status: api_token::ApiTokenStatus::Active,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    state.token_store.create(&mut token).await?;
    Ok((StatusCode::CREATED, Json(token)))
}

async fn update_token(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(updates): Json<serde_json::Value>,
) -> Result<Json<ApiToken>, AppError> {
    let mut existing = state.token_store.get_by_id(id).await?;

    if let Some(name) = updates.get("name").and_then(|v| v.as_str()) {
        existing.name = name.to_string();
    }
    if let Some(allowed) = updates.get("allowed_accounts").and_then(|v| v.as_str()) {
        existing.allowed_accounts = allowed.to_string();
    }
    if let Some(blocked) = updates.get("blocked_accounts").and_then(|v| v.as_str()) {
        existing.blocked_accounts = blocked.to_string();
    }
    if let Some(status) = updates.get("status").and_then(|v| v.as_str()) {
        if !status.is_empty() {
            existing.status = status.to_string().into();
        }
    }

    state.token_store.update(&existing).await?;
    Ok(Json(existing))
}

async fn delete_token_handler(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, AppError> {
    state.token_store.delete(id).await?;
    Ok(Json(serde_json::json!({"status": "deleted"})))
}

// --- Dashboard ---

async fn get_dashboard(State(state): State<AppState>) -> Result<Json<serde_json::Value>, AppError> {
    use crate::service::account::AccountCategory;

    let accounts = state.account_svc.list_accounts().await?;
    let token_count = state.token_store.count().await.unwrap_or(0);

    // 旧字段（为兼容老前端，保留）
    let mut active = 0;
    let mut err_count = 0;
    let mut disabled = 0;

    // 新分类计数（5 类用户视角分类）
    let mut available = 0i64;
    let mut rate_limited = 0i64;
    let mut invalid = 0i64;
    let mut banned = 0i64;
    let mut stopped = 0i64;

    let mut by_platform: std::collections::HashMap<String, i64> = std::collections::HashMap::new();

    for a in &accounts {
        match a.status {
            AccountStatus::Active => active += 1,
            AccountStatus::Error => err_count += 1,
            AccountStatus::Disabled => disabled += 1,
        }
        let key = if a.platform.is_empty() {
            "claude".to_string()
        } else {
            a.platform.clone()
        };
        *by_platform.entry(key).or_insert(0) += 1;

        match state.account_svc.categorize(a).category {
            AccountCategory::Available => available += 1,
            AccountCategory::RateLimited => rate_limited += 1,
            AccountCategory::Invalid => invalid += 1,
            AccountCategory::Banned => banned += 1,
            AccountCategory::Stopped => stopped += 1,
        }
    }

    let total = accounts.len() as f64;
    let schedulable_pct = if total > 0.0 {
        (available as f64) / total
    } else {
        0.0
    };

    Ok(Json(serde_json::json!({
        "accounts": {
            "total": accounts.len(),
            // 旧字段保留 (DB.status 计数)
            "active": active,
            "error": err_count,
            "disabled": disabled,
            "by_platform": by_platform,
            // 新分类（5 类）
            "available": available,
            "rate_limited": rate_limited,
            "invalid": invalid,
            "banned": banned,
            "stopped": stopped,
            "schedulable_pct": schedulable_pct,
        },
        "tokens": token_count,
    })))
}

// --- OAuth Flow Handlers ---

async fn oauth_generate_auth_url(
    State(state): State<AppState>,
    Json(req): Json<crate::service::oauth_flow::GenerateAuthUrlRequest>,
) -> Json<crate::service::oauth_flow::GenerateAuthUrlResponse> {
    Json(state.oauth_flow_svc.generate_auth_url(&req))
}

async fn oauth_generate_setup_token_url(
    State(state): State<AppState>,
    Json(req): Json<crate::service::oauth_flow::GenerateAuthUrlRequest>,
) -> Json<crate::service::oauth_flow::GenerateAuthUrlResponse> {
    Json(state.oauth_flow_svc.generate_setup_token_url(&req))
}

async fn oauth_exchange_code(
    State(state): State<AppState>,
    Json(req): Json<crate::service::oauth_flow::ExchangeCodeRequest>,
) -> Result<Json<crate::service::oauth_flow::ExchangeCodeResponse>, AppError> {
    let resp = state.oauth_flow_svc.exchange_code(&req).await?;
    Ok(Json(resp))
}

async fn oauth_exchange_setup_token_code(
    State(state): State<AppState>,
    Json(req): Json<crate::service::oauth_flow::ExchangeCodeRequest>,
) -> Result<Json<crate::service::oauth_flow::ExchangeCodeResponse>, AppError> {
    let resp = state.oauth_flow_svc.exchange_setup_token_code(&req).await?;
    Ok(Json(resp))
}

// --- SessionKey-based 自动 OAuth Handlers ---

/// POST /admin/accounts/cookie-auth (sub2api 兼容: 只换 token 不建账号)
async fn oauth_cookie_auth(
    State(state): State<AppState>,
    Json(req): Json<crate::service::oauth_flow::CookieAuthRequest>,
) -> Result<Json<crate::service::oauth_flow::CookieAuthResponse>, AppError> {
    let resp = state.oauth_flow_svc.cookie_auth(&req).await?;
    Ok(Json(resp))
}

#[derive(Deserialize, Clone)]
struct CookieAuthCreateRequest {
    session_key: String,
    #[serde(default)]
    proxy_url: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    // 账号字段 (全部可选)
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    priority: Option<i32>,
    #[serde(default)]
    concurrency: Option<i32>,
    #[serde(default)]
    billing_mode: Option<String>,
    #[serde(default)]
    auto_telemetry: Option<bool>,
    #[serde(default)]
    subscription_type: Option<String>,
    /// 1h cache TTL 注入：默认开启（None 时按 true 处理，最大化缓存命中率）。
    /// 显式传 false 可关闭。
    #[serde(default)]
    enable_cache_ttl_1h_injection: Option<bool>,
}

async fn build_account_from_cookie_auth(
    state: &AppState,
    req: &CookieAuthCreateRequest,
) -> Result<Account, AppError> {
    let token = state
        .oauth_flow_svc
        .cookie_auth(&crate::service::oauth_flow::CookieAuthRequest {
            session_key: req.session_key.clone(),
            proxy_url: req.proxy_url.clone(),
            scope: req.scope.clone(),
        })
        .await?;

    let email = token.email_address.clone();
    if email.is_empty() {
        return Err(AppError::Internal(
            "OAuth 成功但未拿到 email_address, 无法创建账号".into(),
        ));
    }
    let name = req.name.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| email.clone());

    let expires_at = if token.expires_at > 0 {
        Utc.timestamp_opt(token.expires_at, 0).single()
    } else {
        None
    };

    let mut account = Account {
        id: 0,
        name,
        email,
        status: AccountStatus::Active,
        auth_type: AccountAuthType::Oauth,
        setup_token: String::new(),
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at,
        oauth_refreshed_at: None,
        auth_error: String::new(),
        proxy_url: req.proxy_url.clone().unwrap_or_default(),
        device_id: String::new(),
        canonical_env: serde_json::json!({}),
        canonical_prompt: serde_json::json!({}),
        canonical_process: serde_json::json!({}),
        billing_mode: req
            .billing_mode
            .clone()
            .unwrap_or_else(|| "strip".into())
            .into(),
        account_uuid: if token.account_uuid.is_empty() {
            None
        } else {
            Some(token.account_uuid)
        },
        organization_uuid: if token.organization_uuid.is_empty() {
            None
        } else {
            Some(token.organization_uuid)
        },
        // 优先用前端显式传入的 subscription_type, 否则用 OAuth 自动检测的档位
        subscription_type: req
            .subscription_type
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                if token.subscription_type.is_empty() {
                    None
                } else {
                    Some(token.subscription_type.clone())
                }
            }),
        concurrency: req.concurrency.unwrap_or(3),
        priority: req.priority.unwrap_or(50),
        rate_limited_at: None,
        rate_limit_reset_at: None,
        disable_reason: String::new(),
        auto_telemetry: req.auto_telemetry.unwrap_or(false),
        telemetry_count: 0,
        experimental_reveal_thinking: false,
        enable_cache_ttl_1h_injection: req.enable_cache_ttl_1h_injection.unwrap_or(true),
        session_key: req.session_key.clone(),
        usage_data: serde_json::json!({}),
        usage_fetched_at: None,
        platform: "claude".into(),
        extra: serde_json::json!({}),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    state.account_svc.create_account(&mut account).await?;
    Ok(account)
}

/// POST /admin/accounts/cookie-auth-create (cc-bridge 自定义: 一步换 token + 建账号)
async fn oauth_cookie_auth_create(
    State(state): State<AppState>,
    Json(req): Json<CookieAuthCreateRequest>,
) -> Result<(StatusCode, Json<Account>), AppError> {
    let account = build_account_from_cookie_auth(&state, &req).await?;
    Ok((StatusCode::CREATED, Json(account)))
}

#[derive(Deserialize)]
struct CookieAuthCreateBatchRequest {
    session_keys: Vec<String>,
    #[serde(default)]
    proxy_url: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    concurrency_limit: Option<usize>,
    // 共享的账号默认值
    #[serde(default)]
    priority: Option<i32>,
    #[serde(default)]
    concurrency: Option<i32>,
    #[serde(default)]
    billing_mode: Option<String>,
    #[serde(default)]
    auto_telemetry: Option<bool>,
    #[serde(default)]
    subscription_type: Option<String>,
    /// 批量导入时所有账号默认是否开启 1h cache TTL 注入。
    /// None → 默认 true（最大化缓存命中率）。显式传 false 可关闭。
    #[serde(default)]
    enable_cache_ttl_1h_injection: Option<bool>,
}

#[derive(serde::Serialize)]
struct BatchItemResult {
    session_key_preview: String,
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    account_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(serde::Serialize)]
struct CookieAuthCreateBatchResponse {
    total: usize,
    success: usize,
    failed: usize,
    results: Vec<BatchItemResult>,
}

fn mask_session_key(sk: &str) -> String {
    let n = sk.chars().count();
    if n <= 18 {
        return "***".to_string();
    }
    let head: String = sk.chars().take(12).collect();
    let tail: String = sk.chars().skip(n - 6).collect();
    format!("{}...{}", head, tail)
}

/// POST /admin/accounts/cookie-auth-create/batch
async fn oauth_cookie_auth_create_batch(
    State(state): State<AppState>,
    Json(req): Json<CookieAuthCreateBatchRequest>,
) -> Result<Json<CookieAuthCreateBatchResponse>, AppError> {
    if req.session_keys.is_empty() {
        return Err(AppError::BadRequest("session_keys 为空".into()));
    }
    let total = req.session_keys.len();
    let limit = req.concurrency_limit.unwrap_or(5).clamp(1, 30);

    let semaphore = Arc::new(tokio::sync::Semaphore::new(limit));
    let mut joins = tokio::task::JoinSet::new();

    for sk in req.session_keys.iter().cloned() {
        let sem = semaphore.clone();
        let state = state.clone();
        let item = CookieAuthCreateRequest {
            session_key: sk.clone(),
            proxy_url: req.proxy_url.clone(),
            scope: req.scope.clone(),
            name: None,
            priority: req.priority,
            concurrency: req.concurrency,
            billing_mode: req.billing_mode.clone(),
            auto_telemetry: req.auto_telemetry,
            subscription_type: req.subscription_type.clone(),
            enable_cache_ttl_1h_injection: req.enable_cache_ttl_1h_injection,
        };
        joins.spawn(async move {
            let _permit = sem.acquire_owned().await.ok();
            let preview = mask_session_key(&sk);
            match build_account_from_cookie_auth(&state, &item).await {
                Ok(account) => BatchItemResult {
                    session_key_preview: preview,
                    success: true,
                    account_id: Some(account.id),
                    email: Some(account.email),
                    error: None,
                },
                Err(e) => BatchItemResult {
                    session_key_preview: preview,
                    success: false,
                    account_id: None,
                    email: None,
                    error: Some(e.to_string()),
                },
            }
        });
    }

    let mut results = Vec::with_capacity(total);
    while let Some(join_res) = joins.join_next().await {
        match join_res {
            Ok(item) => results.push(item),
            Err(e) => results.push(BatchItemResult {
                session_key_preview: "?".into(),
                success: false,
                account_id: None,
                email: None,
                error: Some(format!("task panic: {}", e)),
            }),
        }
    }

    let success = results.iter().filter(|r| r.success).count();
    let failed = total - success;
    Ok(Json(CookieAuthCreateBatchResponse {
        total,
        success,
        failed,
        results,
    }))
}

// --- OpenAI 账号入库 Handlers (Phase 2) ---

#[derive(Deserialize)]
struct OpenAIAccountCreateRequest {
    /// 账号显示名 (默认空, 后端会兜底)
    #[serde(default)]
    name: Option<String>,
    /// 必填: email 标识 (用作 unique key)
    email: String,
    /// 凭证类型: "api_key" / "codex_token" / "oauth" / "cookie" (cookie 仅占位, 短期当 api_key 处理)
    #[serde(default)]
    credential_type: Option<String>,
    /// API Key (sk-* 或 sk-proj-*); 也用于存 codex_token
    #[serde(default)]
    api_key: Option<String>,
    /// OAuth access_token (auth_type=oauth 时用)
    #[serde(default)]
    access_token: Option<String>,
    /// OAuth refresh_token
    #[serde(default)]
    refresh_token: Option<String>,
    /// 自定义 base_url (默认 https://api.openai.com)
    #[serde(default)]
    base_url: Option<String>,
    /// 强制 User-Agent (codex 默认走 codex_cli_rs/0.104.0)
    #[serde(default)]
    user_agent: Option<String>,
    /// ChatGPT account_id (OAuth 账号必填, 走 chatgpt.com 时要)
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    /// organization_id (OpenAI-Organization-Id header 用)
    #[serde(default)]
    organization_id: Option<String>,
    /// 代理
    #[serde(default)]
    proxy_url: Option<String>,
    #[serde(default)]
    priority: Option<i32>,
    #[serde(default)]
    concurrency: Option<i32>,
}

async fn create_openai_account(
    State(state): State<AppState>,
    Json(req): Json<OpenAIAccountCreateRequest>,
) -> Result<(StatusCode, Json<Account>), AppError> {
    if req.email.trim().is_empty() {
        return Err(AppError::BadRequest("email is required".into()));
    }
    let credential_type = req
        .credential_type
        .clone()
        .unwrap_or_else(|| "api_key".into());

    // auth_type 映射: api_key / codex_token / cookie → setup_token (单 token 鉴权)
    //                oauth → oauth (双 token + 自动刷新)
    let auth_type = match credential_type.as_str() {
        "oauth" => AccountAuthType::Oauth,
        _ => AccountAuthType::SetupToken,
    };

    // 构造 extra (平台特有字段塞这里)
    let mut extra = serde_json::Map::new();
    extra.insert("credential_type".into(), serde_json::json!(credential_type));
    if let Some(b) = req.base_url.as_ref().filter(|s| !s.is_empty()) {
        extra.insert("base_url".into(), serde_json::json!(b));
    }
    if let Some(ua) = req.user_agent.as_ref().filter(|s| !s.is_empty()) {
        extra.insert("user_agent".into(), serde_json::json!(ua));
    }
    if let Some(cid) = req.chatgpt_account_id.as_ref().filter(|s| !s.is_empty()) {
        extra.insert("chatgpt_account_id".into(), serde_json::json!(cid));
    }
    if let Some(oid) = req.organization_id.as_ref().filter(|s| !s.is_empty()) {
        extra.insert("organization_id".into(), serde_json::json!(oid));
    }

    // 校验: 至少要一个 token
    let setup_token = req.api_key.clone().unwrap_or_default();
    let access_token = req.access_token.clone().unwrap_or_default();
    if setup_token.is_empty() && access_token.is_empty() {
        return Err(AppError::BadRequest(
            "需提供 api_key (api_key/codex_token 模式) 或 access_token (oauth 模式)".into(),
        ));
    }

    let mut account = Account {
        id: 0,
        name: req
            .name
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| req.email.clone()),
        email: req.email,
        status: AccountStatus::Active,
        auth_type,
        setup_token,
        access_token,
        refresh_token: req.refresh_token.unwrap_or_default(),
        expires_at: None,
        oauth_refreshed_at: None,
        auth_error: String::new(),
        proxy_url: req.proxy_url.unwrap_or_default(),
        device_id: String::new(),
        canonical_env: serde_json::json!({}),
        canonical_prompt: serde_json::json!({}),
        canonical_process: serde_json::json!({}),
        billing_mode: crate::model::account::BillingMode::Strip,
        account_uuid: None,
        organization_uuid: None,
        subscription_type: None,
        concurrency: req.concurrency.unwrap_or(3),
        priority: req.priority.unwrap_or(50),
        rate_limited_at: None,
        rate_limit_reset_at: None,
        disable_reason: String::new(),
        auto_telemetry: false,
        telemetry_count: 0,
        experimental_reveal_thinking: false,
        enable_cache_ttl_1h_injection: false,
        session_key: String::new(),
        usage_data: serde_json::json!({}),
        usage_fetched_at: None,
        platform: "openai".into(),
        extra: serde_json::Value::Object(extra),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    state.account_svc.create_account(&mut account).await?;
    Ok((StatusCode::CREATED, Json(account)))
}

// --- OpenAI 用量探测 (Phase 9) ---
// 从 ChatGPT codex/responses 响应头解析限流信息
// sub2api ParseCodexRateLimitHeaders 移植: 关注 5h 和 7d 滚动窗口
fn parse_codex_rate_limit_headers(
    headers: &reqwest::header::HeaderMap,
) -> Option<serde_json::Value> {
    fn get(h: &reqwest::header::HeaderMap, key: &str) -> Option<String> {
        h.get(key).and_then(|v| v.to_str().ok()).map(String::from)
    }

    let mut updates = serde_json::Map::new();
    let now = chrono::Utc::now().timestamp();
    updates.insert("codex_usage_updated_at".into(), serde_json::json!(now));

    // ChatGPT 响应头里常见的限流字段:
    //   x-codex-primary-used-percent / x-codex-primary-window-minutes / x-codex-primary-reset-after-seconds
    //   x-codex-secondary-used-percent / ...
    //   x-codex-account-rate-limit-* (备用)
    let mut got_any = false;

    if let Some(p) = get(headers, "x-codex-primary-used-percent")
        .and_then(|s| s.parse::<f64>().ok())
    {
        updates.insert("codex_usage_5h_used_percent".into(), serde_json::json!(p));
        got_any = true;
    }
    if let Some(p) = get(headers, "x-codex-primary-window-minutes")
        .and_then(|s| s.parse::<i64>().ok())
    {
        updates.insert("codex_usage_5h_window_minutes".into(), serde_json::json!(p));
        got_any = true;
    }
    if let Some(p) = get(headers, "x-codex-primary-reset-after-seconds")
        .and_then(|s| s.parse::<i64>().ok())
    {
        updates.insert("codex_usage_5h_reset_after_seconds".into(), serde_json::json!(p));
        updates.insert(
            "codex_usage_5h_reset_at".into(),
            serde_json::json!(now + p),
        );
        got_any = true;
    }

    if let Some(p) = get(headers, "x-codex-secondary-used-percent")
        .and_then(|s| s.parse::<f64>().ok())
    {
        updates.insert("codex_usage_7d_used_percent".into(), serde_json::json!(p));
        got_any = true;
    }
    if let Some(p) = get(headers, "x-codex-secondary-window-minutes")
        .and_then(|s| s.parse::<i64>().ok())
    {
        updates.insert("codex_usage_7d_window_minutes".into(), serde_json::json!(p));
        got_any = true;
    }
    if let Some(p) = get(headers, "x-codex-secondary-reset-after-seconds")
        .and_then(|s| s.parse::<i64>().ok())
    {
        updates.insert("codex_usage_7d_reset_after_seconds".into(), serde_json::json!(p));
        updates.insert(
            "codex_usage_7d_reset_at".into(),
            serde_json::json!(now + p),
        );
        got_any = true;
    }

    if got_any {
        Some(serde_json::Value::Object(updates))
    } else {
        None
    }
}

/// 把探测到的 usage 字段 merge 到 account.extra 并落库
async fn persist_codex_usage(
    account_svc: Arc<AccountService>,
    account_id: i64,
    usage_update: serde_json::Value,
) -> Result<(), AppError> {
    let mut account = account_svc.get_account(account_id).await?;
    let mut current = match account.extra.as_object() {
        Some(o) => o.clone(),
        None => serde_json::Map::new(),
    };
    if let Some(updates) = usage_update.as_object() {
        for (k, v) in updates.iter() {
            current.insert(k.clone(), v.clone());
        }
    }
    account.extra = serde_json::Value::Object(current);
    account_svc.update_account(&account).await?;
    Ok(())
}

/// 把 codex_429_until merge 到 account.extra 并落库
async fn persist_codex_429_until(
    account_svc: Arc<AccountService>,
    account_id: i64,
    until_unix: i64,
) -> Result<(), AppError> {
    let mut account = account_svc.get_account(account_id).await?;
    account.extra = crate::service::openai_limit::merge_429_into_extra(&account.extra, until_unix);
    account_svc.update_account(&account).await?;
    Ok(())
}

/// POST /admin/accounts/:id/openai-usage/probe
/// 主动发探测请求拉用量 (复用 chat_completions 同款链路, 但 body 是最小测试请求)
async fn openai_usage_probe(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, AppError> {
    let account = state.account_svc.get_account(id).await?;
    if account.platform != "openai" {
        return Err(AppError::BadRequest("only openai accounts support probe".into()));
    }
    if account.auth_type != AccountAuthType::Oauth {
        return Err(AppError::BadRequest(
            "only OAuth accounts have codex usage; API key accounts have no usage to probe".into(),
        ));
    }
    if account.access_token.is_empty() {
        return Err(AppError::BadRequest("account has no access_token".into()));
    }

    let chatgpt_account_id = account
        .extra
        .as_object()
        .and_then(|m| m.get("chatgpt_account_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let probe_body = serde_json::json!({
        "model": "gpt-5",
        "instructions": "ping",
        "input": [{"role":"user","content":[{"type":"input_text","text":"ping"}]}],
        "max_output_tokens": 1,
        "stream": false,
        "store": false,
    });

    let client = crate::tlsfp::make_request_client(&account.proxy_url);
    let mut up_req = client
        .post("https://chatgpt.com/backend-api/codex/responses")
        .header("Authorization", format!("Bearer {}", account.access_token))
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream")
        .header("Host", "chatgpt.com")
        .header("OpenAI-Beta", "responses=experimental")
        .header("Originator", "codex_cli_rs")
        .header("Version", "0.104.0")
        .header("User-Agent", "codex_cli_rs/0.104.0")
        .json(&probe_body);
    if !chatgpt_account_id.is_empty() {
        up_req = up_req.header("chatgpt-account-id", chatgpt_account_id);
    }

    let resp = up_req
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("probe request failed: {}", e)))?;

    let status = resp.status().as_u16();
    let headers = resp.headers().clone();

    let usage = parse_codex_rate_limit_headers(&headers);
    if let Some(ref u) = usage {
        let _ = persist_codex_usage(state.account_svc.clone(), id, u.clone()).await;
    }

    Ok(Json(serde_json::json!({
        "status": status,
        "usage": usage,
        "found": usage.is_some(),
    })))
}

// --- OpenAI Refresh Token 一键导入 (Phase 7+, 对齐 sub2api 主流模式) ---

#[derive(Deserialize, Clone)]
struct OpenAIRtImportRequest {
    /// 必填: refresh_token (sk-... 或 chatgpt 内部 token)
    refresh_token: String,
    /// 可选: 自定义 email (默认从 id_token 解析)
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    proxy_url: Option<String>,
    #[serde(default)]
    user_agent: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    priority: Option<i32>,
    #[serde(default)]
    concurrency: Option<i32>,
}

async fn build_openai_account_from_rt(
    state: &AppState,
    req: &OpenAIRtImportRequest,
) -> Result<Account, AppError> {
    // 用 refresh_token 跑一次 refresh, 拿 access_token + 解析 id_token
    let proxy = req.proxy_url.as_deref().unwrap_or("");
    let token = state
        .openai_oauth_svc
        .refresh_token(&req.refresh_token, proxy)
        .await?;

    // email 优先用户自定义, 否则从 id_token 解析, 都没有则用 chatgpt_account_id 兜底
    let email = req
        .email
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            if !token.email.is_empty() {
                Some(token.email.clone())
            } else {
                None
            }
        })
        .or_else(|| {
            if !token.chatgpt_account_id.is_empty() {
                Some(format!("chatgpt-{}@openai", token.chatgpt_account_id))
            } else {
                None
            }
        })
        .ok_or_else(|| {
            AppError::Internal("无法确定 email (id_token 没有 email 字段, 请显式传 email)".into())
        })?;

    let mut extra = serde_json::Map::new();
    extra.insert("credential_type".into(), serde_json::json!("oauth_rt"));
    if let Some(b) = req.base_url.as_ref().filter(|s| !s.is_empty()) {
        extra.insert("base_url".into(), serde_json::json!(b));
    }
    if let Some(ua) = req.user_agent.as_ref().filter(|s| !s.is_empty()) {
        extra.insert("user_agent".into(), serde_json::json!(ua));
    }
    if !token.chatgpt_account_id.is_empty() {
        extra.insert(
            "chatgpt_account_id".into(),
            serde_json::json!(token.chatgpt_account_id),
        );
    }
    if !token.organization_id.is_empty() {
        extra.insert(
            "organization_id".into(),
            serde_json::json!(token.organization_id),
        );
    }
    if !token.plan_type.is_empty() {
        extra.insert("plan_type".into(), serde_json::json!(token.plan_type));
    }
    if !token.subscription_expires_at.is_empty() {
        extra.insert(
            "subscription_expires_at".into(),
            serde_json::json!(token.subscription_expires_at),
        );
    }
    if !token.privacy_mode.is_empty() {
        extra.insert("privacy_mode".into(), serde_json::json!(token.privacy_mode));
    }

    let expires_at = Utc.timestamp_opt(token.expires_at, 0).single();
    let mut account = Account {
        id: 0,
        name: email.clone(),
        email,
        status: AccountStatus::Active,
        auth_type: AccountAuthType::Oauth,
        setup_token: String::new(),
        access_token: token.access_token,
        refresh_token: if token.refresh_token.is_empty() {
            req.refresh_token.clone()
        } else {
            token.refresh_token
        },
        expires_at,
        oauth_refreshed_at: None,
        auth_error: String::new(),
        proxy_url: req.proxy_url.clone().unwrap_or_default(),
        device_id: String::new(),
        canonical_env: serde_json::json!({}),
        canonical_prompt: serde_json::json!({}),
        canonical_process: serde_json::json!({}),
        billing_mode: crate::model::account::BillingMode::Strip,
        account_uuid: None,
        organization_uuid: None,
        subscription_type: None,
        concurrency: req.concurrency.unwrap_or(3),
        priority: req.priority.unwrap_or(50),
        rate_limited_at: None,
        rate_limit_reset_at: None,
        disable_reason: String::new(),
        auto_telemetry: false,
        telemetry_count: 0,
        experimental_reveal_thinking: false,
        enable_cache_ttl_1h_injection: false,
        session_key: String::new(),
        usage_data: serde_json::json!({}),
        usage_fetched_at: None,
        platform: "openai".into(),
        extra: serde_json::Value::Object(extra),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    state.account_svc.create_account(&mut account).await?;
    Ok(account)
}

/// POST /admin/accounts/openai-rt-import (单个 refresh_token)
async fn openai_rt_import(
    State(state): State<AppState>,
    Json(req): Json<OpenAIRtImportRequest>,
) -> Result<(StatusCode, Json<Account>), AppError> {
    let account = build_openai_account_from_rt(&state, &req).await?;
    Ok((StatusCode::CREATED, Json(account)))
}

#[derive(Deserialize)]
struct OpenAIRtImportBatchRequest {
    refresh_tokens: Vec<String>,
    #[serde(default)]
    proxy_url: Option<String>,
    #[serde(default)]
    user_agent: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    concurrency_limit: Option<usize>,
    #[serde(default)]
    priority: Option<i32>,
    #[serde(default)]
    concurrency: Option<i32>,
}

#[derive(serde::Serialize)]
struct OpenAIRtBatchItem {
    rt_preview: String,
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    account_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(serde::Serialize)]
struct OpenAIRtBatchResponse {
    total: usize,
    success: usize,
    failed: usize,
    results: Vec<OpenAIRtBatchItem>,
}

/// POST /admin/accounts/openai-rt-import/batch (多个 refresh_token, 并发控制)
async fn openai_rt_import_batch(
    State(state): State<AppState>,
    Json(req): Json<OpenAIRtImportBatchRequest>,
) -> Result<Json<OpenAIRtBatchResponse>, AppError> {
    if req.refresh_tokens.is_empty() {
        return Err(AppError::BadRequest("refresh_tokens 为空".into()));
    }
    let total = req.refresh_tokens.len();
    let limit = req.concurrency_limit.unwrap_or(3).clamp(1, 10);

    let semaphore = Arc::new(tokio::sync::Semaphore::new(limit));
    let mut joins = tokio::task::JoinSet::new();

    for rt in req.refresh_tokens.iter().cloned() {
        let sem = semaphore.clone();
        let state = state.clone();
        let item = OpenAIRtImportRequest {
            refresh_token: rt.clone(),
            email: None,
            proxy_url: req.proxy_url.clone(),
            user_agent: req.user_agent.clone(),
            base_url: req.base_url.clone(),
            priority: req.priority,
            concurrency: req.concurrency,
        };
        joins.spawn(async move {
            let _permit = sem.acquire_owned().await.ok();
            let preview = mask_session_key(&rt);
            match build_openai_account_from_rt(&state, &item).await {
                Ok(account) => OpenAIRtBatchItem {
                    rt_preview: preview,
                    success: true,
                    account_id: Some(account.id),
                    email: Some(account.email),
                    error: None,
                },
                Err(e) => OpenAIRtBatchItem {
                    rt_preview: preview,
                    success: false,
                    account_id: None,
                    email: None,
                    error: Some(e.to_string()),
                },
            }
        });
    }

    let mut results = Vec::with_capacity(total);
    while let Some(j) = joins.join_next().await {
        match j {
            Ok(item) => results.push(item),
            Err(e) => results.push(OpenAIRtBatchItem {
                rt_preview: "?".into(),
                success: false,
                account_id: None,
                email: None,
                error: Some(format!("task panic: {}", e)),
            }),
        }
    }

    let success = results.iter().filter(|r| r.success).count();
    let failed = total - success;
    Ok(Json(OpenAIRtBatchResponse {
        total,
        success,
        failed,
        results,
    }))
}

// --- OpenAI OAuth Handlers (Phase 4) ---

async fn openai_oauth_generate_auth_url(
    State(state): State<AppState>,
    Json(req): Json<crate::service::openai_oauth::OpenAIGenerateAuthUrlRequest>,
) -> Json<crate::service::openai_oauth::OpenAIGenerateAuthUrlResponse> {
    Json(state.openai_oauth_svc.generate_auth_url(&req))
}

async fn openai_oauth_exchange_code(
    State(state): State<AppState>,
    Json(req): Json<crate::service::openai_oauth::OpenAIExchangeCodeRequest>,
) -> Result<Json<crate::service::openai_oauth::OpenAIExchangeCodeResponse>, AppError> {
    let resp = state.openai_oauth_svc.exchange_code(&req).await?;
    Ok(Json(resp))
}

#[derive(Deserialize)]
struct OpenAIRefreshTokenRequest {
    refresh_token: String,
    #[serde(default)]
    proxy_url: Option<String>,
}

async fn openai_oauth_refresh_token(
    State(state): State<AppState>,
    Json(req): Json<OpenAIRefreshTokenRequest>,
) -> Result<Json<crate::service::openai_oauth::OpenAIExchangeCodeResponse>, AppError> {
    let proxy = req.proxy_url.as_deref().unwrap_or("");
    let resp = state
        .openai_oauth_svc
        .refresh_token(&req.refresh_token, proxy)
        .await?;
    Ok(Json(resp))
}

// --- 健康检查 + Metrics ---

/// `/livez` — 进程是否存活。永远 200,只要这条 handler 能跑就说明 axum / tokio 没死。
async fn livez_handler() -> Response {
    (StatusCode::OK, "ok").into_response()
}

/// `/readyz` — 是否准备好接业务流量。需要 DB ping 通过。
async fn readyz_handler(State(state): State<AppState>) -> Response {
    // 同时 ping DB 和 cache; 任一不通就报 503, 让 LB 把流量切走
    if let Err(e) = state.account_svc.ping_db().await {
        return (StatusCode::SERVICE_UNAVAILABLE, format!("db: {}", e)).into_response();
    }
    if let Err(e) = state.cache.ping().await {
        return (StatusCode::SERVICE_UNAVAILABLE, format!("cache: {}", e)).into_response();
    }
    (StatusCode::OK, "ready").into_response()
}

/// `/metrics` — Prometheus 文本格式指标暴露。
async fn metrics_handler(State(state): State<AppState>) -> Response {
    use crate::model::account::AccountStatus;
    use crate::service::metrics::{AccountsByStatus, METRICS};

    // 即时读账号状态分布 (从 list_schedulable cache + DB 拿;失败时 gauges 全 0)
    let mut acc = AccountsByStatus::default();
    if let Ok(accounts) = state.account_svc.list_accounts().await {
        for a in &accounts {
            let claude = a.platform.is_empty() || a.platform == "claude";
            let openai = a.platform == "openai";
            match a.status {
                AccountStatus::Active => {
                    if claude {
                        acc.claude_active += 1;
                    } else if openai {
                        acc.openai_active += 1;
                    }
                }
                AccountStatus::Error => {
                    if claude {
                        acc.claude_error += 1;
                    } else if openai {
                        acc.openai_error += 1;
                    }
                }
                AccountStatus::Disabled => {
                    if claude {
                        acc.claude_disabled += 1;
                    } else if openai {
                        acc.openai_disabled += 1;
                    }
                }
            }
        }
    }

    let body = METRICS.render(&acc);
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4")],
        body,
    )
        .into_response()
}

// --- 内嵌前端静态资源 ---

#[derive(Embed)]
#[folder = "web/dist"]
struct Assets;

/// SPA 页面：返回 index.html
async fn spa_handler() -> impl IntoResponse {
    match Assets::get("index.html") {
        Some(index) => Response::builder()
            .header("content-type", "text/html")
            .body(axum::body::Body::from(index.data.to_vec()))
            .unwrap(),
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(axum::body::Body::from("frontend not built"))
            .unwrap(),
    }
}

/// 前端静态资源：/assets/*
async fn asset_handler(req: Request) -> impl IntoResponse {
    let path = req.uri().path().trim_start_matches('/');
    if let Some(file) = Assets::get(path) {
        let mime = mime_from_path(path);
        return Response::builder()
            .header("content-type", mime)
            .body(axum::body::Body::from(file.data.to_vec()))
            .unwrap();
    }
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(axum::body::Body::from("not found"))
        .unwrap()
}

fn mime_from_path(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html",
        Some("css") => "text/css",
        Some("js") => "application/javascript",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        _ => "application/octet-stream",
    }
}

fn timestamp_millis_to_utc(ts: i64) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::Utc.timestamp_millis_opt(ts).single()
}

fn client_datetime_to_utc(value: &ClientDateTime) -> Option<DateTime<Utc>> {
    match value {
        ClientDateTime::Millis(ts) => timestamp_millis_to_utc(*ts),
        ClientDateTime::Text(text) => parse_client_datetime_str(text),
    }
}

fn client_datetime_value_to_utc(value: &serde_json::Value) -> Option<DateTime<Utc>> {
    match value {
        serde_json::Value::Number(n) => n.as_i64().and_then(timestamp_millis_to_utc),
        serde_json::Value::String(s) => parse_client_datetime_str(s),
        _ => None,
    }
}

fn parse_client_datetime_str(value: &str) -> Option<DateTime<Utc>> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(ms) = trimmed.parse::<i64>() {
        return timestamp_millis_to_utc(ms);
    }
    DateTime::parse_from_rfc3339(trimmed)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_client_datetime_millis_number() {
        let parsed = client_datetime_value_to_utc(&serde_json::json!(1775737845123_i64)).unwrap();
        assert_eq!(parsed.timestamp(), 1775737845);
        assert_eq!(parsed.timestamp_subsec_millis(), 123);
    }

    #[test]
    fn parses_client_datetime_millis_string() {
        let parsed = client_datetime_value_to_utc(&serde_json::json!("1775737845123")).unwrap();
        assert_eq!(parsed.timestamp(), 1775737845);
        assert_eq!(parsed.timestamp_subsec_millis(), 123);
    }

    #[test]
    fn parses_client_datetime_rfc3339_string() {
        let parsed =
            client_datetime_value_to_utc(&serde_json::json!("2026-04-09T12:30:45Z")).unwrap();
        assert_eq!(parsed.timestamp(), 1775737845);
    }

    #[test]
    fn empty_client_datetime_string_becomes_none() {
        assert!(client_datetime_value_to_utc(&serde_json::json!("")).is_none());
    }
}
