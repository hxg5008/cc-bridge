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

const SCHEMA_VERSION: i32 = 2;

/// 连接池配置: max_connections 决定网关并发能跑多高的 DB QPS。
/// 50 是单实例 1500-2000 RPS 场景下的工程取舍 —— PG 默认 max_connections=100,
/// 留一半给其他客户端 (admin tooling / pg_dump 等)。
const DB_POOL_MAX_CONNECTIONS: u32 = 50;
/// 最少保持几个空闲连接, 避免冷启动峰值排队。
const DB_POOL_MIN_CONNECTIONS: u32 = 5;
/// 拿连接的最大等待时长。超时直接错, 不阻塞 tokio worker。
const DB_POOL_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn init_db(dsn: &str) -> Result<AnyPool, sqlx::Error> {
    let pool = AnyPoolOptions::new()
        .max_connections(DB_POOL_MAX_CONNECTIONS)
        .min_connections(DB_POOL_MIN_CONNECTIONS)
        .acquire_timeout(DB_POOL_ACQUIRE_TIMEOUT)
        .connect(dsn)
        .await?;
    info!(
        "db pool: max={} min={} acquire_timeout={:?}",
        DB_POOL_MAX_CONNECTIONS, DB_POOL_MIN_CONNECTIONS, DB_POOL_ACQUIRE_TIMEOUT
    );
    Ok(pool)
}

pub async fn ensure_postgres_database(cfg: &DatabaseConfig) -> Result<(), String> {
    if cfg.has_explicit_dsn() {
        return Ok(());
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

    let pending: [(&str, &str); 18] = [
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
