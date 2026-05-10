use claude_code_gateway::config;
use claude_code_gateway::handler;
use claude_code_gateway::service;
use claude_code_gateway::store;

use std::sync::Arc;
use tracing::info;

/// Tokio worker 线程数显式配置:
///   `std::thread::available_parallelism()` 自 Rust 1.59 在 Linux 上调
///   `sched_getaffinity` —— cgroup-aware, 与容器 CPU limit 对齐, 不会
///   读到宿主机物理核数。但仍可被 `TOKIO_WORKER_THREADS` 环境变量覆盖
///   (压测调参 / docker --cpus 限制不准时手动指定)。
///
/// 最少 2 worker: 单核机器降级到完全串行, runtime 内一个慢 future 会饿死所有 IO。
fn worker_threads() -> usize {
    if let Ok(s) = std::env::var("TOKIO_WORKER_THREADS") {
        if let Ok(n) = s.parse::<usize>() {
            if n > 0 {
                return n;
            }
        }
    }
    let n = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    n.max(2)
}

fn main() {
    // CLI flags: --version / -V (升级脚本检查当前版本用)
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("cc-bridge {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!(
            "cc-bridge {}\n\nUsage: claude-code-gateway [--version|--help]\n\
             Configuration is via environment variables (see .env.example).",
            env!("CARGO_PKG_VERSION")
        );
        return;
    }

    let workers = worker_threads();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .expect("build tokio runtime failed");

    runtime.block_on(async move {
        run(workers).await;
    });
}

async fn run(workers: usize) {
    let cfg = config::Config::load();

    // 初始化日志
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| cfg.log_level.clone().into()),
        )
        .init();

    info!("tokio runtime: {} worker threads", workers);

    // 弱口令拒启动: ADMIN_PASSWORD 必须达到最低安全要求, 否则 admin 面板会被
    // 暴力穷举 / 凭证填充攻击。若需临时放行 (本地开发调试) 可设环境变量
    // CCBRIDGE_ALLOW_WEAK_ADMIN_PASSWORD=1 跳过, 但生产不要这么做。
    enforce_strong_admin_password(&cfg.admin.password);

    // 标记启动时间, 供 /metrics ccbridge_uptime_seconds gauge 使用
    service::metrics::METRICS.mark_started();

    // 注册 sqlx Any 驱动
    sqlx::any::install_default_drivers();

    // 初始化数据库（PostgreSQL only）
    cfg.database.driver(); // 早期校验：拒绝 sqlite 等不再支持的 driver
    store::db::ensure_postgres_database(&cfg.database)
        .await
        .expect("prepare postgres failed");
    let dsn = cfg.database.dsn();
    info!("database: postgres ({})", dsn);

    let pool = store::db::init_db(&dsn).await.expect("init db failed");
    store::db::migrate(&pool).await.expect("migrate failed");

    // 防御性约束：禁止 billing_mode='rewrite'（破坏缓存命中）
    // escape: 设 CCBRIDGE_ALLOW_REWRITE 环境变量
    store::db::ensure_no_rewrite_constraint(&pool).await;

    // 缓存：优先 Redis，回退内存
    let cache: Arc<dyn store::cache::CacheStore> = match &cfg.redis {
        Some(redis_cfg) => {
            match store::redis::RedisStore::new(
                &redis_cfg.host,
                redis_cfg.port,
                &redis_cfg.password,
                redis_cfg.db,
            )
            .await
            {
                Ok(r) => {
                    info!("using redis cache");
                    Arc::new(r)
                }
                Err(e) => {
                    info!("redis unavailable ({}), using in-memory cache", e);
                    let m = Arc::new(store::memory::MemoryStore::new());
                    m.spawn_gc();
                    m
                }
            }
        }
        None => {
            info!("no redis configured, using in-memory cache");
            let m = Arc::new(store::memory::MemoryStore::new());
            m.spawn_gc();
            m
        }
    };

    let account_store = Arc::new(store::account_store::AccountStore::new(pool.clone()));
    let token_store = Arc::new(store::token_store::TokenStore::new(pool.clone()));

    // 一次性清理：Phase 1 之前旧限流路径写入的残留字段（status='active' 账号上的
    // rate_limited_at / rate_limit_reset_at / disable_reason）。幂等，每次启动执行。
    match account_store.clear_stale_rate_limit_fields().await {
        Ok(n) if n > 0 => tracing::info!("cleared stale rate-limit fields on {} account(s)", n),
        Ok(_) => {}
        Err(e) => tracing::warn!("clear stale rate-limit fields failed: {}", e),
    }

    let limit_store = Arc::new(service::limit::LimitStore::new(account_store.clone()));

    // 启动时从 DB 把 usage_data hydrate 进 LimitStore 内存,
    // 否则重启后内存空, dashboard 会把全部 100% 的限流号显示为"可用"。
    if let Ok(accs) = account_store.list().await {
        let mut hydrated = 0u32;
        for a in &accs {
            if a.usage_data.is_object() && !a.usage_data.as_object().map(|o| o.is_empty()).unwrap_or(true) {
                limit_store.ingest_usage_json(a.id, &a.usage_data);
                hydrated += 1;
            }
        }
        info!("hydrated LimitStore from DB: {} accounts", hydrated);
    }

    let oauth_flow_svc = Arc::new(service::oauth_flow::OAuthFlowService::new());

    let account_svc = Arc::new(
        service::account::AccountService::new(
            account_store.clone(),
            cache.clone(),
            limit_store.clone(),
        )
        .with_oauth_flow(oauth_flow_svc.clone()),
    );
    let rewriter = Arc::new(service::rewriter::Rewriter::new());
    let telemetry_svc = Arc::new(service::telemetry::TelemetryService::new(
        account_store.clone(),
        account_svc.clone(),
    ));
    let gateway_svc = Arc::new(service::gateway::GatewayService::new(
        account_svc.clone(),
        rewriter.clone(),
        telemetry_svc.clone(),
        limit_store.clone(),
    ));
    let token_tester = Arc::new(service::oauth::TokenTester::new());
    let openai_oauth_svc = Arc::new(service::openai_oauth::OpenAIOAuthService::new());

    let app = handler::router::build_router(
        &cfg,
        gateway_svc,
        account_svc,
        token_tester,
        token_store,
        oauth_flow_svc,
        openai_oauth_svc,
        telemetry_svc,
        cache.clone(),
    );

    let addr = format!("{}:{}", cfg.server.host, cfg.server.port);
    // 警告: cc-bridge 自身永远走 plain HTTP, 即使配置了 tls_cert/tls_key 也只是占位。
    // 商用必须前置 nginx / caddy / 云 LB 做 TLS 终结 (deploy/Caddyfile.example 已附),
    // 否则客户端 token 与上游 OAuth Bearer 全程明文走公网。
    info!("claude-code-gateway listening on http://{} (PLAIN HTTP — must be fronted by HTTPS reverse proxy in production)", addr);
    if cfg.server.tls_cert.is_some() {
        tracing::warn!(
            "TLS_CERT_FILE/TLS_KEY_FILE 配置已被忽略: 当前版本不内置 TLS, 必须前置反代; 见 deploy/Caddyfile.example"
        );
    }

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    let limit_store_for_shutdown = limit_store.clone();
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal_and_flush(limit_store_for_shutdown));

    // 硬 deadline: 收到信号后总共最多等 45s, 超时强制退出
    // (graceful drain 30s 留给 SSE 流自然结束 + 15s 给 flush_all)
    match tokio::time::timeout(std::time::Duration::from_secs(45), server).await {
        Ok(Ok(())) => info!("clean exit"),
        Ok(Err(e)) => tracing::error!("serve error: {}", e),
        Err(_) => tracing::warn!(
            "graceful shutdown exceeded 45s, exiting forcefully (in-flight SSE may drop)"
        ),
    }
}

/// 监听 SIGTERM / Ctrl-C, 收到信号后:
///   1. 先 flush LimitStore 内存里的所有账号状态到 DB (防限流状态丢失)
///   2. 再返回, 触发 axum graceful shutdown (开始 drain in-flight 请求)
///
/// 部署时 (docker stop / k8s rolling update / systemctl stop) 会先发 SIGTERM,
/// 默认 grace period 10s 之后才 SIGKILL — 我们这边只要在 grace period 内把
/// 当前响应流写完就行 (外层有 45s 硬 deadline 兜底)。
async fn shutdown_signal_and_flush(limit_store: std::sync::Arc<crate::service::limit::LimitStore>) {
    use tokio::signal;

    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("install Ctrl+C handler failed");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler failed")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            info!("received Ctrl-C");
        },
        _ = terminate => {
            info!("received SIGTERM");
        },
    }

    // 收到关停信号: 先 flush 限流状态 (15s 硬 deadline), 再返回触发 axum drain
    info!("flushing LimitStore to DB before shutdown...");
    let flush_fut = limit_store.flush_all();
    if tokio::time::timeout(std::time::Duration::from_secs(15), flush_fut).await.is_err() {
        tracing::warn!("flush_all exceeded 15s, some limit state may be lost");
    }
    info!("draining in-flight requests...");
}

/// 弱口令检查: ADMIN_PASSWORD 在以下情况会拒启动:
///   1. 长度 < 8 字符 (任何形式都太短)
///   2. 出现在公开弱口令黑名单 (admin / password / 123456 / claude 等)
///
/// 设环境变量 `CCBRIDGE_ALLOW_WEAK_ADMIN_PASSWORD=1` 可临时放行 (本地调试用,
/// **生产环境绝不要设**)。
///
/// 实测开销 < 1ms 一次, 仅启动时调一次。
fn enforce_strong_admin_password(password: &str) {
    if std::env::var("CCBRIDGE_ALLOW_WEAK_ADMIN_PASSWORD").unwrap_or_default() == "1" {
        tracing::warn!(
            "CCBRIDGE_ALLOW_WEAK_ADMIN_PASSWORD=1, 跳过弱口令检查 — 仅本地调试可用, 生产风险极高"
        );
        return;
    }

    let char_len = password.chars().count();
    if char_len < 8 {
        eprintln!(
            "FATAL: ADMIN_PASSWORD 太短 ({}个字符), 至少需要 8 个字符。\n\
             生产环境强烈建议 16+ 字符的随机串。\n\
             示例: openssl rand -base64 24 | tr -d '=' | tr '/+' '-_' | head -c 32\n\
             临时放行: export CCBRIDGE_ALLOW_WEAK_ADMIN_PASSWORD=1 (仅限本地调试)",
            char_len
        );
        std::process::exit(2);
    }

    // ASCII 检查: HTTP header value 按 RFC 7230 必须是 ASCII。
    // 包含中文 / Emoji / 非 ASCII 字符的密码客户端能存, 但 header 传输时
    // axum::HeaderValue::to_str() 会拒绝, 导致 admin 永远 401 — 谁也登不上。
    if !password.is_ascii() {
        eprintln!(
            "FATAL: ADMIN_PASSWORD 包含非 ASCII 字符 (例如中文 / emoji)。\n\
             HTTP header 不支持非 ASCII, 客户端发出 x-api-key header 时会被服务端\n\
             解析失败, 导致 admin 永远 401 谁也登不上。\n\
             请改成纯 ASCII 字符 (大小写字母 + 数字 + 常见标点)。\n\
             示例: openssl rand -base64 24 | tr -d '=' | tr '/+' '-_' | head -c 32"
        );
        std::process::exit(2);
    }

    // 弱口令黑名单 (来自 SecLists rockyou top + 项目相关常见值)
    const WEAK_PASSWORDS: &[&str] = &[
        "admin",
        "administrator",
        "password",
        "passw0rd",
        "12345678",
        "123456789",
        "1234567890",
        "qwerty",
        "qwertyuiop",
        "abc12345",
        "letmein",
        "changeme",
        "secret",
        "default",
        "root",
        "toor",
        "test",
        "test1234",
        "claude",
        "anthropic",
        "cc-bridge",
        "ccbridge",
        "openai",
        "p@ssw0rd",
        "password1",
        "iloveyou",
        "welcome",
        "admin123",
        "11111111",
        "00000000",
    ];
    let lower = password.to_lowercase();
    if WEAK_PASSWORDS.iter().any(|w| lower == *w) {
        eprintln!(
            "FATAL: ADMIN_PASSWORD 命中常见弱口令黑名单, 拒绝启动。\n\
             请改成至少 16 字符的随机串。\n\
             示例: openssl rand -base64 24 | tr -d '=' | tr '/+' '-_' | head -c 32"
        );
        std::process::exit(2);
    }

    // 长度警告 (不致命): 8-15 字符也允许但提示运维加强
    if char_len < 16 {
        tracing::warn!(
            "ADMIN_PASSWORD 长度 {} 字符 < 推荐 16 字符, 商用环境请考虑加强",
            char_len
        );
    }
}
