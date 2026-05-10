use crate::config::DatabaseConfig;
use sqlx::AnyPool;
use sqlx::Connection;
use sqlx::any::AnyPoolOptions;
use sqlx::postgres::PgConnection;
use std::collections::HashSet;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use tracing::info;

const SCHEMA_VERSION: i32 = 4;

/// 连接池配置: max_connections 决定网关并发能跑多高的 DB QPS。
/// 100 是几百用户商用场景下的取舍: SSE 长连不占 DB 连接, 真正占用的是
/// auth 校验 + flush_to_db (有 Semaphore(8) 限并发) + admin API。
/// 提前到 100 给 burst 留余量, 配合 Semaphore + LRU 缓存基本不会满。
/// PG 默认 max_connections=100 时这里要降到 80, 生产环境建议把 PG 调到 200。
const DB_POOL_MAX_CONNECTIONS: u32 = 100;
/// 最少保持几个空闲连接, 避免冷启动峰值排队。
const DB_POOL_MIN_CONNECTIONS: u32 = 10;
/// 拿连接的最大等待时长。超时直接错, 不阻塞 tokio worker。
const DB_POOL_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(10);
/// 单条连接最长生命: 30 分钟后强制 reconnect, 防 PG 端 TCP 连接老化 / NAT 断开。
const DB_POOL_MAX_LIFETIME: Duration = Duration::from_secs(30 * 60);
/// 空闲连接最大保留时长: 10 分钟没用就回收, 节省 PG backend 进程。
const DB_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

pub async fn init_db(dsn: &str) -> Result<AnyPool, sqlx::Error> {
    let pool = AnyPoolOptions::new()
        .max_connections(DB_POOL_MAX_CONNECTIONS)
        .min_connections(DB_POOL_MIN_CONNECTIONS)
        .acquire_timeout(DB_POOL_ACQUIRE_TIMEOUT)
        .max_lifetime(DB_POOL_MAX_LIFETIME)
        .idle_timeout(DB_POOL_IDLE_TIMEOUT)
        .connect(dsn)
        .await?;
    info!(
        "db pool: max={} min={} acquire_timeout={:?} max_lifetime={:?} idle_timeout={:?}",
        DB_POOL_MAX_CONNECTIONS,
        DB_POOL_MIN_CONNECTIONS,
        DB_POOL_ACQUIRE_TIMEOUT,
        DB_POOL_MAX_LIFETIME,
        DB_POOL_IDLE_TIMEOUT
    );
    Ok(pool)
}

pub async fn ensure_postgres_database(cfg: &DatabaseConfig) -> Result<(), String> {
    if cfg.has_explicit_dsn() {
        return Ok(());
    }

    // 自动 docker compose up postgres 仅在显式 dev 模式下启用。
    // 生产环境 PG 通常是独立部署 (RDS / 自建服务), 不应该让 cc-bridge 二进制去
    // 调用 docker compose; 那样要么找不到 docker (报错让人困惑), 要么真的起一个
    // 跟生产 DB 同名但隔离的本地实例 (数据脑裂)。
    // 触发条件: 必须显式 export CCBRIDGE_DEV=1
    if std::env::var("CCBRIDGE_DEV").unwrap_or_default() != "1" {
        return Err(
            "DATABASE_DSN 未设置且 CCBRIDGE_DEV != 1; 生产环境请在 .env 配置 DATABASE_DSN \
             指向独立部署的 PostgreSQL (例如 postgres://user:pass@db-host:5432/cc-bridge), \
             不要依赖容器内自动 docker compose"
                .to_string(),
        );
    }

    if !Path::new("/.dockerenv").exists() {
        start_compose_postgres()?;
    } else {
        info!("DATABASE_DSN not set, using compose postgres service");
    }

    wait_for_postgres(cfg).await?;
    create_database_if_missing(cfg).await?;
    Ok(())
}

pub async fn migrate(pool: &AnyPool) -> Result<(), sqlx::Error> {
    // Fast path: if schema_migrations records the current version, skip everything.
    sqlx::query("CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY)")
        .execute(pool)
        .await?;
    let applied: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM schema_migrations WHERE version >= {}",
        SCHEMA_VERSION
    ))
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    if applied > 0 {
        return Ok(());
    }

    // 多实例同时启动时, advisory lock 串行化 migration, 防止两实例都跑 ALTER /
    // 数据 UPDATE 翻倍。
    //
    // **关键 (修 A1)**: PG advisory lock 是 session-level — 用 pool.execute() 后
    // 连接立刻归还池, lock 跟着 session 结束失效。必须 acquire 一条专属连接,
    // 在这条连接上 lock → migrate → unlock, 全程不归还。
    //
    // 如果 acquire 失败 (理论上 SQLite 模式不走这里; 但 fail-soft 仍要让 SQLite
    // 单实例本地开发能跑), 退化为旧行为 (无 lock 直接迁移)。
    let mut conn = match pool.acquire().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                "failed to acquire dedicated connection for migration ({}); falling back without lock",
                e
            );
            return run_migrations_inner(pool).await;
        }
    };

    // 在专属连接上拿 lock。AnyConnection 的 execute API: 用 &mut *conn 解 PoolConnection
    let lock_ok = sqlx::query("SELECT pg_advisory_lock(hashtext('cc_bridge_migrate'))")
        .execute(&mut *conn)
        .await
        .is_ok();
    if !lock_ok {
        tracing::warn!(
            "pg_advisory_lock failed (likely SQLite mode); migration proceeds without serialization"
        );
        // 释放连接, fall back 用 pool 跑迁移
        drop(conn);
        return run_migrations_inner(pool).await;
    }

    // 拿锁后重新检查 schema_version (可能在等锁的几百毫秒里, 另一实例已经跑完)
    let applied2: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM schema_migrations WHERE version >= {}",
        SCHEMA_VERSION
    ))
    .fetch_one(&mut *conn)
    .await
    .unwrap_or(0);
    if applied2 > 0 {
        let _ = sqlx::query("SELECT pg_advisory_unlock(hashtext('cc_bridge_migrate'))")
            .execute(&mut *conn)
            .await;
        return Ok(());
    }

    // 真正跑迁移: run_migrations_inner 仍用 pool, 因为它要并发跑 ALTER (其实不会,
    // 但接口不变最省事)。这期间 lock 仍由 conn 持有, 其他实例的 advisory_lock
    // 会阻塞等待。
    let result = run_migrations_inner(pool).await;

    // 不论成败都要 release lock; conn drop 时 PG session 结束 lock 也会自动释放,
    // 显式 unlock 是好习惯让连接可以放回池。
    let _ = sqlx::query("SELECT pg_advisory_unlock(hashtext('cc_bridge_migrate'))")
        .execute(&mut *conn)
        .await;
    drop(conn);
    result
}

async fn run_migrations_inner(pool: &AnyPool) -> Result<(), sqlx::Error> {

    for stmt in PG_SCHEMA.split(';') {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        sqlx::query(stmt).execute(pool).await?;
    }

    // api_tokens 表 — create before column-existence probing so both tables are present.
    for stmt in PG_TOKENS_SCHEMA.split(';') {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        sqlx::query(stmt).execute(pool).await?;
    }

    // 增量迁移 — only ALTER columns that are actually missing, so remote-DB startups
    // don't pay ~20 round-trips for ALTERs that would otherwise fail with "column
    // already exists" and get swallowed by .ok().
    let cols = existing_columns(pool, "accounts").await;

    let pending: [(&str, &str); 20] = [
        (
            "billing_mode",
            "ALTER TABLE accounts ADD COLUMN billing_mode TEXT NOT NULL DEFAULT 'strip'",
        ),
        (
            "usage_data",
            "ALTER TABLE accounts ADD COLUMN usage_data JSONB NOT NULL DEFAULT '{}'",
        ),
        (
            "usage_fetched_at",
            "ALTER TABLE accounts ADD COLUMN usage_fetched_at TIMESTAMPTZ",
        ),
        (
            "auth_type",
            "ALTER TABLE accounts ADD COLUMN auth_type TEXT NOT NULL DEFAULT 'setup_token'",
        ),
        (
            "access_token",
            "ALTER TABLE accounts ADD COLUMN access_token TEXT NOT NULL DEFAULT ''",
        ),
        (
            "refresh_token",
            "ALTER TABLE accounts ADD COLUMN refresh_token TEXT NOT NULL DEFAULT ''",
        ),
        (
            "oauth_expires_at",
            "ALTER TABLE accounts ADD COLUMN oauth_expires_at TIMESTAMPTZ",
        ),
        (
            "oauth_refreshed_at",
            "ALTER TABLE accounts ADD COLUMN oauth_refreshed_at TIMESTAMPTZ",
        ),
        (
            "auth_error",
            "ALTER TABLE accounts ADD COLUMN auth_error TEXT NOT NULL DEFAULT ''",
        ),
        (
            "account_uuid",
            "ALTER TABLE accounts ADD COLUMN account_uuid TEXT",
        ),
        (
            "organization_uuid",
            "ALTER TABLE accounts ADD COLUMN organization_uuid TEXT",
        ),
        (
            "subscription_type",
            "ALTER TABLE accounts ADD COLUMN subscription_type TEXT",
        ),
        (
            "disable_reason",
            "ALTER TABLE accounts ADD COLUMN disable_reason TEXT NOT NULL DEFAULT ''",
        ),
        (
            "auto_telemetry",
            "ALTER TABLE accounts ADD COLUMN auto_telemetry INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "telemetry_count",
            "ALTER TABLE accounts ADD COLUMN telemetry_count INTEGER NOT NULL DEFAULT 0",
        ),
        // Phase 1 (multi-platform): 加 platform 字段, 默认 claude (向后兼容现有账号)
        (
            "platform",
            "ALTER TABLE accounts ADD COLUMN platform TEXT NOT NULL DEFAULT 'claude'",
        ),
        // Phase 1: 加 extra JSONB 字段, 平台特有数据 (openai_passthrough / ua_override 等)
        (
            "extra",
            "ALTER TABLE accounts ADD COLUMN extra JSONB NOT NULL DEFAULT '{}'",
        ),
        // 上游 v1.8.6: experimental_reveal_thinking per-account toggle
        (
            "experimental_reveal_thinking",
            "ALTER TABLE accounts ADD COLUMN experimental_reveal_thinking INTEGER NOT NULL DEFAULT 0",
        ),
        // 1h cache TTL injection per-account toggle (移植自 sub2api v0.1.121)
        // 默认 0 (关闭)，开启后对 OAuth/SetupToken 账号的 /v1/messages 请求注入 ttl="1h"
        (
            "enable_cache_ttl_1h_injection",
            "ALTER TABLE accounts ADD COLUMN enable_cache_ttl_1h_injection INTEGER NOT NULL DEFAULT 0",
        ),
        // session_key from claude.ai cookie，用于 refresh_token 失效时自动重新换 token
        // 明文存储；空字符串 = 没有 recovery 能力（老账号）
        (
            "session_key",
            "ALTER TABLE accounts ADD COLUMN session_key TEXT NOT NULL DEFAULT ''",
        ),
    ];
    for (name, sql) in pending.iter() {
        if !cols.contains(*name) {
            sqlx::query(sql).execute(pool).await.ok();
        }
    }

    // Fix column types for existing PG databases that may have TEXT instead of TIMESTAMPTZ/JSONB.
    // Only run when the current data_type doesn't already match.
    let types = column_types(pool, "accounts").await;
    let needs_type = |col: &str, want: &str| {
        types
            .get(col)
            .map(|t| !t.eq_ignore_ascii_case(want))
            .unwrap_or(false)
    };
    if needs_type("usage_data", "jsonb") {
        sqlx::query("ALTER TABLE accounts ALTER COLUMN usage_data TYPE JSONB USING usage_data::JSONB")
            .execute(pool)
            .await
            .ok();
    }
    if needs_type("usage_fetched_at", "timestamp with time zone") {
        sqlx::query("ALTER TABLE accounts ALTER COLUMN usage_fetched_at TYPE TIMESTAMPTZ USING usage_fetched_at::TIMESTAMPTZ")
            .execute(pool)
            .await
            .ok();
    }
    if needs_type("oauth_expires_at", "timestamp with time zone") {
        sqlx::query("ALTER TABLE accounts ALTER COLUMN oauth_expires_at TYPE TIMESTAMPTZ USING oauth_expires_at::TIMESTAMPTZ")
            .execute(pool)
            .await
            .ok();
    }
    if needs_type("oauth_refreshed_at", "timestamp with time zone") {
        sqlx::query("ALTER TABLE accounts ALTER COLUMN oauth_refreshed_at TYPE TIMESTAMPTZ USING oauth_refreshed_at::TIMESTAMPTZ")
            .execute(pool)
            .await
            .ok();
    }

    // Stamp the version last so a partial failure above causes a clean retry next boot.
    sqlx::query(&format!(
        "INSERT INTO schema_migrations (version) VALUES ({}) ON CONFLICT DO NOTHING",
        SCHEMA_VERSION
    ))
    .execute(pool)
    .await
    .ok();

    Ok(())
}

/// 幂等地确保 accounts 表上有 CHECK 约束禁止 billing_mode='rewrite'。
/// 防御性措施：rewrite 模式会把 cch_hash 注入到 system 块的 billing header 行，
/// 每次请求 hash 都不同 → system 前缀漂移 → Anthropic 缓存命中率为 0。
/// 强制账号必须用 strip 模式才能保证缓存稳定。
///
/// Escape hatch：设置环境变量 `CCBRIDGE_ALLOW_REWRITE=1` 跳过此检查
/// （仅当 Anthropic 加严反指纹检测、必须靠 rewrite 通过校验时使用）。
pub async fn ensure_no_rewrite_constraint(pool: &AnyPool) {
    if std::env::var("CCBRIDGE_ALLOW_REWRITE").is_ok() {
        info!("CCBRIDGE_ALLOW_REWRITE set, skipping no-rewrite constraint enforcement");
        return;
    }

    let constraint_name = "accounts_billing_mode_no_rewrite";
    // 查约束是否已存在
    let exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_constraint WHERE conname = $1",
    )
    .bind(constraint_name)
    .fetch_one(pool)
    .await
    .unwrap_or(0);

    if exists > 0 {
        return;
    }

    // 清理已有的 rewrite 账号（幂等：UPDATE 0 行也无害）
    let cleared = sqlx::query("UPDATE accounts SET billing_mode='strip' WHERE billing_mode='rewrite'")
        .execute(pool)
        .await;
    if let Ok(r) = &cleared {
        if r.rows_affected() > 0 {
            tracing::warn!(
                "cleared {} accounts from billing_mode='rewrite' to 'strip' (rewrite mode breaks cache hit rate)",
                r.rows_affected()
            );
        }
    }

    // 加 CHECK 约束
    let alter_sql = format!(
        "ALTER TABLE accounts ADD CONSTRAINT {} CHECK (billing_mode != 'rewrite')",
        constraint_name
    );
    match sqlx::query(&alter_sql).execute(pool).await {
        Ok(_) => info!("added CHECK constraint {}: billing_mode='rewrite' is now blocked at DB level", constraint_name),
        Err(e) => tracing::warn!("failed to add CHECK constraint {}: {} (continuing)", constraint_name, e),
    }
}

async fn existing_columns(pool: &AnyPool, table: &str) -> HashSet<String> {
    let sql = format!(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = '{}'",
        table.replace('\'', "''")
    );
    sqlx::query_scalar::<_, String>(&sql)
        .fetch_all(pool)
        .await
        .unwrap_or_default()
        .into_iter()
        .collect()
}

async fn column_types(pool: &AnyPool, table: &str) -> std::collections::HashMap<String, String> {
    let sql = format!(
        "SELECT column_name, data_type FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = '{}'",
        table.replace('\'', "''")
    );
    match sqlx::query_as::<_, (String, String)>(&sql)
        .fetch_all(pool)
        .await
    {
        Ok(rows) => rows.into_iter().collect(),
        Err(_) => std::collections::HashMap::new(),
    }
}

const PG_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS accounts (
    id              BIGSERIAL PRIMARY KEY,
    name            TEXT NOT NULL DEFAULT '',
    email           TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'active',
    token           TEXT NOT NULL,
    auth_type       TEXT NOT NULL DEFAULT 'setup_token',
    access_token    TEXT NOT NULL DEFAULT '',
    refresh_token   TEXT NOT NULL DEFAULT '',
    oauth_expires_at    TIMESTAMPTZ,
    oauth_refreshed_at  TIMESTAMPTZ,
    auth_error      TEXT NOT NULL DEFAULT '',
    proxy_url       TEXT NOT NULL DEFAULT '',
    device_id       TEXT NOT NULL,
    canonical_env   JSONB NOT NULL DEFAULT '{}',
    canonical_prompt_env JSONB NOT NULL DEFAULT '{}',
    canonical_process    JSONB NOT NULL DEFAULT '{}',
    billing_mode    TEXT NOT NULL DEFAULT 'strip',
    concurrency     INT NOT NULL DEFAULT 3,
    priority        INT NOT NULL DEFAULT 50,
    rate_limited_at      TIMESTAMPTZ,
    rate_limit_reset_at  TIMESTAMPTZ,
    account_uuid         TEXT,
    organization_uuid    TEXT,
    subscription_type    TEXT,
    disable_reason       TEXT NOT NULL DEFAULT '',
    auto_telemetry       INT NOT NULL DEFAULT 0,
    telemetry_count      BIGINT NOT NULL DEFAULT 0,
    experimental_reveal_thinking INT NOT NULL DEFAULT 0,
    enable_cache_ttl_1h_injection INT NOT NULL DEFAULT 0,
    session_key          TEXT NOT NULL DEFAULT '',
    usage_data           JSONB NOT NULL DEFAULT '{}',
    usage_fetched_at     TIMESTAMPTZ,
    platform        TEXT NOT NULL DEFAULT 'claude',
    extra           JSONB NOT NULL DEFAULT '{}',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

"#;

const PG_TOKENS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS api_tokens (
    id                  BIGSERIAL PRIMARY KEY,
    name                TEXT NOT NULL DEFAULT '',
    token               TEXT NOT NULL UNIQUE,
    allowed_accounts    TEXT NOT NULL DEFAULT '',
    blocked_accounts    TEXT NOT NULL DEFAULT '',
    status              TEXT NOT NULL DEFAULT 'active',
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
)
"#;

fn start_compose_postgres() -> Result<(), String> {
    info!("DATABASE_DSN not set, starting postgres via docker compose");
    let output = Command::new("docker")
        .args(["compose", "up", "-d", "postgres"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .map_err(|err| format!("failed to run docker compose: {err}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Err(format!(
        "docker compose up -d postgres failed: {}",
        if !stderr.is_empty() { stderr } else { stdout }
    ))
}

async fn wait_for_postgres(cfg: &DatabaseConfig) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let admin_dsn = cfg.admin_dsn();
    let mut last_error = String::new();

    while Instant::now() < deadline {
        match PgConnection::connect(&admin_dsn).await {
            Ok(_) => {
                info!("postgres is ready at {}:{}", cfg.host, cfg.port);
                return Ok(());
            }
            Err(err) => {
                last_error = err.to_string();
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }

    Err(format!(
        "postgres did not become ready within 60s ({}:{}){}",
        cfg.host,
        cfg.port,
        if last_error.is_empty() {
            String::new()
        } else {
            format!(": {last_error}")
        }
    ))
}

async fn create_database_if_missing(cfg: &DatabaseConfig) -> Result<(), String> {
    let mut conn = PgConnection::connect(&cfg.admin_dsn())
        .await
        .map_err(|err| format!("failed to connect to postgres admin database: {err}"))?;

    let exists = sqlx::query_scalar::<_, i64>("SELECT 1 FROM pg_database WHERE datname = $1")
        .bind(&cfg.dbname)
        .fetch_optional(&mut conn)
        .await
        .map_err(|err| format!("failed to check database existence: {err}"))?
        .is_some();

    if exists {
        info!("postgres database {} already exists", cfg.dbname);
        return Ok(());
    }

    let create_sql = format!("CREATE DATABASE \"{}\"", cfg.dbname.replace('"', "\"\""));
    sqlx::query(&create_sql)
        .execute(&mut conn)
        .await
        .map_err(|err| format!("failed to create database {}: {err}", cfg.dbname))?;
    info!("created postgres database {}", cfg.dbname);
    Ok(())
}
