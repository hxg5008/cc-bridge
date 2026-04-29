use claude_code_gateway::config;
use claude_code_gateway::handler;
use claude_code_gateway::service;
use claude_code_gateway::store;

use std::sync::Arc;
use tracing::info;

/// Tokio worker 线程数显式配置:
/// - 默认 (`#[tokio::main]` 不带参数) 是 `available_parallelism()` 即逻辑核数,
///   但在 docker / cgroups / numa-pinning 下可能误报机器物理核数, 导致 worker 过多。
/// - 显式 = `num_cpus::get()` 读 cgroup-aware 的有效核数, 跟容器 CPU limit 对齐。
/// - 可通过 `TOKIO_WORKER_THREADS` 环境变量进一步覆盖 (运行时压测调参用)。
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
    n.max(2) // 最少 2 worker, 避免单核机降级到完全串行
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
                    Arc::new(store::memory::MemoryStore::new())
                }
            }
        }
        None => {
            info!("no redis configured, using in-memory cache");
            Arc::new(store::memory::MemoryStore::new())
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

    let account_svc = Arc::new(service::account::AccountService::new(
        account_store.clone(),
        cache.clone(),
        limit_store.clone(),
    ));
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
    let oauth_flow_svc = Arc::new(service::oauth_flow::OAuthFlowService::new());
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
    );

    let addr = format!("{}:{}", cfg.server.host, cfg.server.port);
    if cfg.server.tls_cert.is_some() {
        info!("claude-code-gateway listening on https://{}", addr);
    } else {
        info!("claude-code-gateway listening on http://{}", addr);
    }

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
}

/// 监听 SIGTERM / Ctrl-C, 触发后 axum 停止 accept 新连接,等 in-flight 请求自然结束。
///
/// 部署时 (docker stop / k8s rolling update / systemctl stop) 会先发 SIGTERM,
/// 默认 grace period 10s 之后才 SIGKILL — 我们这边只要在 grace period 内把
/// 当前响应流写完就行。
async fn shutdown_signal() {
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
            info!("received Ctrl-C, draining in-flight requests");
        },
        _ = terminate => {
            info!("received SIGTERM, draining in-flight requests");
        },
    }
}
