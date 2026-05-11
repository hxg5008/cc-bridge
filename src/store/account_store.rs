use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::Value;
use sqlx::AnyPool;
use sqlx::Row;
use sqlx::any::AnyRow;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use crate::error::AppError;
use crate::model::account::{Account, AccountStatus};

const SCHEDULABLE_TTL: Duration = Duration::from_secs(30);

pub struct AccountStore {
    pool: AnyPool,
    schedulable_cache: Arc<RwLock<Option<(Instant, Vec<Account>)>>>,
}

impl AccountStore {
    pub fn new(pool: AnyPool) -> Self {
        Self {
            pool,
            schedulable_cache: Arc::new(RwLock::new(None)),
        }
    }

    async fn invalidate_schedulable_cache(&self) {
        let mut guard = self.schedulable_cache.write().await;
        *guard = None;
    }

    /// 数据库连通性检查 (健康端点 /readyz 用)。
    pub async fn ping(&self) -> Result<(), AppError> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    /// Fast path for sticky-session lookups: find an account in the cached
    /// schedulable list without hitting the DB. Returns None on cache miss
    /// or expired cache.
    pub async fn get_schedulable_cached(&self, id: i64) -> Option<Account> {
        let guard = self.schedulable_cache.read().await;
        if let Some((at, ref v)) = *guard {
            if at.elapsed() < SCHEDULABLE_TTL {
                return v.iter().find(|a| a.id == id).cloned();
            }
        }
        None
    }

    fn now_expr(&self) -> &str {
        "NOW()"
    }

    fn fmt_time(&self, t: DateTime<Utc>) -> String {
        t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
    }

    /// Returns `$N::TEXT` for Postgres to work around sqlx Any driver encoding
    /// all NULLs as `Option::<i32>::None` / INT4 type OID. The explicit `::TEXT`
    /// cast makes the INT4→TEXT conversion succeed before assignment to a TEXT column.
    fn nullable(&self, n: u32) -> String {
        format!("${}::TEXT", n)
    }

    /// Like `nullable()` but for TIMESTAMPTZ columns. Uses `$N::TEXT::TIMESTAMPTZ`
    /// for Postgres because TEXT has no implicit assignment cast to TIMESTAMPTZ.
    fn nullable_ts(&self, n: u32) -> String {
        format!("${}::TEXT::TIMESTAMPTZ", n)
    }

    /// Returns `$N::JSONB` for Postgres. The Any driver sends String as TEXT type,
    /// but TEXT has no implicit assignment cast to JSONB.
    fn jsonb(&self, n: u32) -> String {
        format!("${}::JSONB", n)
    }

    fn parse_datetime_str(s: &str) -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(s)
            .map(|dt| dt.with_timezone(&Utc))
            .ok()
            .or_else(|| {
                NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ")
                    .map(|n| n.and_utc())
                    .ok()
            })
            .or_else(|| {
                // PG TIMESTAMPTZ::text 完整格式: "2026-04-16 12:30:45.123456+08:00"
                DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f%:z")
                    .map(|dt| dt.with_timezone(&Utc))
                    .ok()
            })
            .or_else(|| {
                // PG TIMESTAMPTZ::text 短时区格式: "2026-04-16 12:30:45+08"
                DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%#z")
                    .map(|dt| dt.with_timezone(&Utc))
                    .ok()
            })
    }

    fn parse_time(row: &AnyRow, col: &str) -> DateTime<Utc> {
        if let Ok(s) = row.try_get::<String, _>(col) {
            Self::parse_datetime_str(&s).unwrap_or_default()
        } else {
            Utc::now()
        }
    }

    fn parse_optional_time(row: &AnyRow, col: &str) -> Option<DateTime<Utc>> {
        if let Ok(s) = row.try_get::<Option<String>, _>(col) {
            s.and_then(|s| Self::parse_datetime_str(&s))
        } else {
            None
        }
    }

    fn parse_json(row: &AnyRow, col: &str) -> Value {
        if let Ok(s) = row.try_get::<String, _>(col) {
            serde_json::from_str(&s).unwrap_or_else(|_| Value::Object(Default::default()))
        } else {
            Value::Object(Default::default())
        }
    }

    fn select_account_cols(&self) -> &'static str {
        ACCOUNT_COLS_PG_TEXT
    }

    fn returning_account_timestamps(&self) -> &'static str {
        "id, created_at::text AS created_at, updated_at::text AS updated_at"
    }

    fn row_to_account(row: &AnyRow) -> Account {
        Account {
            id: row.try_get::<i64, _>("id").unwrap_or_default(),
            name: row.try_get::<String, _>("name").unwrap_or_default(),
            email: row.try_get::<String, _>("email").unwrap_or_default(),
            status: row
                .try_get::<String, _>("status")
                .unwrap_or_else(|_| "active".into())
                .into(),
            auth_type: row
                .try_get::<String, _>("auth_type")
                .unwrap_or_else(|_| "setup_token".into())
                .into(),
            setup_token: row.try_get::<String, _>("token").unwrap_or_default(),
            access_token: row.try_get::<String, _>("access_token").unwrap_or_default(),
            refresh_token: row
                .try_get::<String, _>("refresh_token")
                .unwrap_or_default(),
            expires_at: Self::parse_optional_time(row, "oauth_expires_at"),
            oauth_refreshed_at: Self::parse_optional_time(row, "oauth_refreshed_at"),
            auth_error: row.try_get::<String, _>("auth_error").unwrap_or_default(),
            proxy_url: row.try_get::<String, _>("proxy_url").unwrap_or_default(),
            device_id: row.try_get::<String, _>("device_id").unwrap_or_default(),
            canonical_env: Self::parse_json(row, "canonical_env"),
            canonical_prompt: Self::parse_json(row, "canonical_prompt_env"),
            canonical_process: Self::parse_json(row, "canonical_process"),
            billing_mode: row
                .try_get::<String, _>("billing_mode")
                .unwrap_or_else(|_| "strip".into())
                .into(),
            account_uuid: row
                .try_get::<Option<String>, _>("account_uuid")
                .unwrap_or(None),
            organization_uuid: row
                .try_get::<Option<String>, _>("organization_uuid")
                .unwrap_or(None),
            subscription_type: row
                .try_get::<Option<String>, _>("subscription_type")
                .unwrap_or(None),
            concurrency: row.try_get::<i32, _>("concurrency").unwrap_or(3),
            priority: row.try_get::<i32, _>("priority").unwrap_or(50),
            rate_limited_at: Self::parse_optional_time(row, "rate_limited_at"),
            rate_limit_reset_at: Self::parse_optional_time(row, "rate_limit_reset_at"),
            disable_reason: row
                .try_get::<String, _>("disable_reason")
                .unwrap_or_default(),
            auto_telemetry: row.try_get::<i32, _>("auto_telemetry").unwrap_or(0) != 0,
            telemetry_count: row.try_get::<i64, _>("telemetry_count").unwrap_or(0),
            experimental_reveal_thinking: row
                .try_get::<i32, _>("experimental_reveal_thinking")
                .unwrap_or(0)
                != 0,
            enable_cache_ttl_1h_injection: row
                .try_get::<i32, _>("enable_cache_ttl_1h_injection")
                .unwrap_or(0)
                != 0,
            session_key: row
                .try_get::<String, _>("session_key")
                .unwrap_or_default(),
            usage_data: Self::parse_json(row, "usage_data"),
            usage_fetched_at: Self::parse_optional_time(row, "usage_fetched_at"),
            platform: row
                .try_get::<String, _>("platform")
                .unwrap_or_else(|_| "claude".to_string()),
            extra: Self::parse_json(row, "extra"),
            created_at: Self::parse_time(row, "created_at"),
            updated_at: Self::parse_time(row, "updated_at"),
        }
    }

    pub async fn create(&self, a: &mut Account) -> Result<(), AppError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) as cnt FROM accounts WHERE email=$1")
            .bind(&a.email)
            .fetch_one(&self.pool)
            .await
            .unwrap_or(0);

        if count > 0 {
            return Err(AppError::BadRequest(format!(
                "email {} already exists",
                a.email
            )));
        }

        let env_str = serde_json::to_string(&a.canonical_env).unwrap_or_else(|_| "{}".into());
        let prompt_str = serde_json::to_string(&a.canonical_prompt).unwrap_or_else(|_| "{}".into());
        let process_str =
            serde_json::to_string(&a.canonical_process).unwrap_or_else(|_| "{}".into());
        let expires_at = a.expires_at.map(|t| self.fmt_time(t));
        let oauth_refreshed_at = a.oauth_refreshed_at.map(|t| self.fmt_time(t));

        let auto_telemetry_int: i32 = if a.auto_telemetry { 1 } else { 0 };
        let extra_str = serde_json::to_string(&a.extra).unwrap_or_else(|_| "{}".into());
        let experimental_reveal_thinking_int: i32 = if a.experimental_reveal_thinking { 1 } else { 0 };
        let enable_cache_ttl_1h_int: i32 = if a.enable_cache_ttl_1h_injection { 1 } else { 0 };
        let q = format!(
            r#"INSERT INTO accounts (name, email, status, token, proxy_url,
                auth_type, access_token, refresh_token, oauth_expires_at, oauth_refreshed_at, auth_error,
                device_id, canonical_env, canonical_prompt_env, canonical_process,
                billing_mode, account_uuid, organization_uuid, subscription_type,
                concurrency, priority, auto_telemetry, platform, extra, experimental_reveal_thinking,
                enable_cache_ttl_1h_injection, session_key)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,{},{},{},$12,{},{},{},$16,{},{},{},$20,$21,$22,$23,{},$25,$26,$27)
            RETURNING {}"#,
            self.nullable_ts(9),
            self.nullable_ts(10),
            "$11",
            self.jsonb(13),
            self.jsonb(14),
            self.jsonb(15),
            self.nullable(17),
            self.nullable(18),
            self.nullable(19),
            self.jsonb(24),
            self.returning_account_timestamps()
        );
        let row: AnyRow = sqlx::query(&q)
            .bind(&a.name)
            .bind(&a.email)
            .bind(a.status.to_string())
            .bind(&a.setup_token)
            .bind(&a.proxy_url)
            .bind(a.auth_type.to_string())
            .bind(&a.access_token)
            .bind(&a.refresh_token)
            .bind(expires_at)
            .bind(oauth_refreshed_at)
            .bind(&a.auth_error)
            .bind(&a.device_id)
            .bind(&env_str)
            .bind(&prompt_str)
            .bind(&process_str)
            .bind(a.billing_mode.to_string())
            .bind(&a.account_uuid)
            .bind(&a.organization_uuid)
            .bind(&a.subscription_type)
            .bind(a.concurrency)
            .bind(a.priority)
            .bind(auto_telemetry_int)
            .bind(&a.platform)
            .bind(&extra_str)
            .bind(experimental_reveal_thinking_int)
            .bind(enable_cache_ttl_1h_int)
            .bind(&a.session_key)
            .fetch_one(&self.pool)
            .await?;

        a.id = row.try_get::<i64, _>("id").unwrap_or_default();
        a.created_at = Self::parse_time(&row, "created_at");
        a.updated_at = Self::parse_time(&row, "updated_at");
        self.invalidate_schedulable_cache().await;
        Ok(())
    }

    pub async fn update(&self, a: &Account) -> Result<(), AppError> {
        let expires_at = a.expires_at.map(|t| self.fmt_time(t));
        let oauth_refreshed_at = a.oauth_refreshed_at.map(|t| self.fmt_time(t));
        let auto_telemetry_int: i32 = if a.auto_telemetry { 1 } else { 0 };
        let extra_str = serde_json::to_string(&a.extra).unwrap_or_else(|_| "{}".into());
        let experimental_reveal_thinking_int: i32 = if a.experimental_reveal_thinking { 1 } else { 0 };
        let enable_cache_ttl_1h_int: i32 = if a.enable_cache_ttl_1h_injection { 1 } else { 0 };
        let q = format!(
            r#"UPDATE accounts SET name=$1, email=$2, status=$3, token=$4,
                auth_type=$5, access_token=$6, refresh_token=$7, oauth_expires_at={}, oauth_refreshed_at={},
                auth_error=$10, proxy_url=$11, billing_mode=$12,
                account_uuid={}, organization_uuid={}, subscription_type={},
                concurrency=$16, priority=$17, auto_telemetry=$18,
                experimental_reveal_thinking=$19, platform=$21, extra={}, enable_cache_ttl_1h_injection=$23, session_key=$24, updated_at={}
            WHERE id=$20"#,
            self.nullable_ts(8),
            self.nullable_ts(9),
            self.nullable(13),
            self.nullable(14),
            self.nullable(15),
            self.jsonb(22),
            self.now_expr()
        );
        sqlx::query(&q)
            .bind(&a.name)
            .bind(&a.email)
            .bind(a.status.to_string())
            .bind(&a.setup_token)
            .bind(a.auth_type.to_string())
            .bind(&a.access_token)
            .bind(&a.refresh_token)
            .bind(expires_at)
            .bind(oauth_refreshed_at)
            .bind(&a.auth_error)
            .bind(&a.proxy_url)
            .bind(a.billing_mode.to_string())
            .bind(&a.account_uuid)
            .bind(&a.organization_uuid)
            .bind(&a.subscription_type)
            .bind(a.concurrency)
            .bind(a.priority)
            .bind(auto_telemetry_int)
            .bind(experimental_reveal_thinking_int)
            .bind(a.id)
            .bind(&a.platform)
            .bind(&extra_str)
            .bind(enable_cache_ttl_1h_int)
            .bind(&a.session_key)
            .execute(&self.pool)
            .await?;
        self.invalidate_schedulable_cache().await;
        Ok(())
    }

    pub async fn update_oauth_tokens(
        &self,
        id: i64,
        access_token: &str,
        refresh_token: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<(), AppError> {
        let q = format!(
            r#"UPDATE accounts SET access_token=$1, refresh_token=$2, oauth_expires_at={},
                oauth_refreshed_at={}, auth_error='', updated_at={}
            WHERE id=$5"#,
            self.nullable_ts(3),
            self.nullable_ts(4),
            self.now_expr()
        );
        sqlx::query(&q)
            .bind(access_token)
            .bind(refresh_token)
            .bind(self.fmt_time(expires_at))
            .bind(self.fmt_time(Utc::now()))
            .bind(id)
            .execute(&self.pool)
            .await?;
        self.invalidate_schedulable_cache().await;
        Ok(())
    }

    pub async fn update_auth_error(&self, id: i64, auth_error: &str) -> Result<(), AppError> {
        let q = format!(
            "UPDATE accounts SET auth_error=$1, updated_at={} WHERE id=$2",
            self.now_expr()
        );
        sqlx::query(&q)
            .bind(auth_error)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn update_status(&self, id: i64, status: AccountStatus) -> Result<(), AppError> {
        let q = format!(
            "UPDATE accounts SET status=$1, updated_at={} WHERE id=$2",
            self.now_expr()
        );
        sqlx::query(&q)
            .bind(status.to_string())
            .bind(id)
            .execute(&self.pool)
            .await?;
        self.invalidate_schedulable_cache().await;
        Ok(())
    }

    pub async fn set_rate_limit(&self, id: i64, reset_at: DateTime<Utc>) -> Result<(), AppError> {
        let q = format!(
            "UPDATE accounts SET rate_limited_at={}, rate_limit_reset_at={}, updated_at={} WHERE id=$3",
            self.nullable_ts(1),
            self.nullable_ts(2),
            self.now_expr()
        );
        sqlx::query(&q)
            .bind(self.fmt_time(Utc::now()))
            .bind(self.fmt_time(reset_at))
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn disable_account(
        &self,
        id: i64,
        status: AccountStatus,
        reason: &str,
        rate_limit_reset_at: Option<DateTime<Utc>>,
    ) -> Result<(), AppError> {
        let q = format!(
            r#"UPDATE accounts SET status=$1, disable_reason=$2,
                rate_limited_at={}, rate_limit_reset_at={}, updated_at={}
            WHERE id=$5"#,
            self.nullable_ts(3),
            self.nullable_ts(4),
            self.now_expr()
        );
        let limited_str = rate_limit_reset_at.map(|_| self.fmt_time(Utc::now()));
        let reset_str = rate_limit_reset_at.map(|t| self.fmt_time(t));
        sqlx::query(&q)
            .bind(status.to_string())
            .bind(reason)
            .bind(limited_str)
            .bind(reset_str)
            .bind(id)
            .execute(&self.pool)
            .await?;
        self.invalidate_schedulable_cache().await;
        Ok(())
    }

    pub async fn enable_account(&self, id: i64) -> Result<(), AppError> {
        let q = format!(
            r#"UPDATE accounts SET status='active', disable_reason='',
                rate_limited_at=NULL, rate_limit_reset_at=NULL, updated_at={}
            WHERE id=$1"#,
            self.now_expr()
        );
        sqlx::query(&q).bind(id).execute(&self.pool).await?;
        self.invalidate_schedulable_cache().await;
        Ok(())
    }

    pub async fn clear_rate_limit(&self, id: i64) -> Result<(), AppError> {
        let q = format!(
            "UPDATE accounts SET rate_limited_at=NULL, rate_limit_reset_at=NULL, updated_at={} WHERE id=$1",
            self.now_expr()
        );
        sqlx::query(&q).bind(id).execute(&self.pool).await?;
        Ok(())
    }

    /// 软恢复机制 — 仅写 extra 一列。invalidate schedulable cache 让 cooldown
    /// 写入立刻对调度器生效。
    pub async fn update_extra(&self, id: i64, extra: serde_json::Value) -> Result<(), AppError> {
        let extra_str = serde_json::to_string(&extra).map_err(|e| {
            AppError::Internal(format!("serialize extra: {}", e))
        })?;
        let q = format!(
            "UPDATE accounts SET extra=$1, updated_at={} WHERE id=$2",
            self.now_expr()
        );
        sqlx::query(&q)
            .bind(extra_str)
            .bind(id)
            .execute(&self.pool)
            .await?;
        self.invalidate_schedulable_cache().await;
        Ok(())
    }

    /// 一次性清理：Phase 1 前由旧限流路径写入 status='active' 账号上的残留字段。
    /// 手动停用（status='disabled'）的账号不动。幂等：清完后二次运行无副作用。
    pub async fn clear_stale_rate_limit_fields(&self) -> Result<u64, AppError> {
        let q = format!(
            r#"UPDATE accounts
               SET rate_limited_at=NULL, rate_limit_reset_at=NULL, disable_reason='',
                   updated_at={}
               WHERE status='active' AND rate_limited_at IS NOT NULL"#,
            self.now_expr()
        );
        let rows = sqlx::query(&q).execute(&self.pool).await?.rows_affected();
        Ok(rows)
    }

    pub async fn delete(&self, id: i64) -> Result<(), AppError> {
        sqlx::query("DELETE FROM accounts WHERE id=$1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        self.invalidate_schedulable_cache().await;
        Ok(())
    }

    pub async fn get_by_id(&self, id: i64) -> Result<Account, AppError> {
        let row: AnyRow = sqlx::query(&format!(
            "SELECT {} FROM accounts WHERE id=$1",
            self.select_account_cols()
        ))
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(Self::row_to_account(&row))
    }

    pub async fn list(&self) -> Result<Vec<Account>, AppError> {
        let rows: Vec<AnyRow> = sqlx::query(&format!(
            "SELECT {} FROM accounts ORDER BY priority ASC, id ASC",
            self.select_account_cols()
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(Self::row_to_account).collect())
    }

    pub async fn count(&self) -> Result<i64, AppError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts")
            .fetch_one(&self.pool)
            .await
            .unwrap_or(0);
        Ok(count)
    }

    pub async fn list_paged(&self, page: i64, page_size: i64) -> Result<Vec<Account>, AppError> {
        let offset = (page - 1) * page_size;
        // 修 Y4: ORDER BY priority + id, priority 可被 admin 并发修改导致幻读 (OFFSET
        // 翻页天然不安全)。包一层 REPEATABLE READ 事务, 让单次查询拿到一致快照,
        // 防止两个 admin 同时翻页 + 改 priority 时出现"第二页重复显示第一页末项"。
        // 注意: tx 仅覆盖单次查询, 跨页(用户翻到下一页)时会用新事务, 跨页期间仍可能
        // 看到 priority 调整后的顺序变化, 这是 OFFSET 翻页的固有限制 — 真正零幻读
        // 需要 keyset (id > last_id) 翻页, 留作后续 admin UI 改造。
        let q = format!(
            "SELECT {} FROM accounts ORDER BY priority ASC, id ASC LIMIT $1 OFFSET $2",
            self.select_account_cols()
        );
        let mut tx = self.pool.begin().await?;
        // PG 默认 READ COMMITTED; 升到 REPEATABLE READ 拿一致性快照
        let _ = sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *tx)
            .await; // SQLite/SQLite-Any 不支持也无害
        let rows: Vec<AnyRow> = sqlx::query(&q)
            .bind(page_size)
            .bind(offset)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(rows.iter().map(Self::row_to_account).collect())
    }

    pub async fn update_usage(&self, id: i64, usage_data: &str) -> Result<(), AppError> {
        let q = format!(
            "UPDATE accounts SET usage_data={}, usage_fetched_at={}, updated_at={} WHERE id=$3",
            self.jsonb(1),
            self.nullable_ts(2),
            self.now_expr()
        );
        sqlx::query(&q)
            .bind(usage_data)
            .bind(self.fmt_time(Utc::now()))
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn increment_telemetry_count(&self, id: i64, delta: i64) -> Result<(), AppError> {
        let q = format!(
            "UPDATE accounts SET telemetry_count = telemetry_count + $1, updated_at={} WHERE id=$2",
            self.now_expr()
        );
        sqlx::query(&q)
            .bind(delta)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_schedulable(&self) -> Result<Vec<Account>, AppError> {
        {
            let guard = self.schedulable_cache.read().await;
            if let Some((at, ref v)) = *guard {
                if at.elapsed() < SCHEDULABLE_TTL {
                    return Ok(v.clone());
                }
            }
        }
        let q = format!(
            r#"SELECT {} FROM accounts
            WHERE status='active'
            ORDER BY priority ASC, id ASC"#,
            self.select_account_cols(),
        );
        let rows: Vec<AnyRow> = sqlx::query(&q).fetch_all(&self.pool).await?;
        let v: Vec<Account> = rows.iter().map(Self::row_to_account).collect();
        {
            let mut guard = self.schedulable_cache.write().await;
            *guard = Some((Instant::now(), v.clone()));
        }
        Ok(v)
    }
}

const ACCOUNT_COLS_PG_TEXT: &str = r#"id, name, email, status, token, auth_type, access_token, refresh_token,
    oauth_expires_at::text AS oauth_expires_at, oauth_refreshed_at::text AS oauth_refreshed_at,
    auth_error, proxy_url, device_id,
    canonical_env::text AS canonical_env, canonical_prompt_env::text AS canonical_prompt_env,
    canonical_process::text AS canonical_process,
    billing_mode, account_uuid, organization_uuid, subscription_type,
    concurrency, priority, rate_limited_at::text AS rate_limited_at,
    rate_limit_reset_at::text AS rate_limit_reset_at,
    disable_reason, auto_telemetry, telemetry_count, experimental_reveal_thinking,
    enable_cache_ttl_1h_injection, session_key,
    usage_data::text AS usage_data, usage_fetched_at::text AS usage_fetched_at,
    platform, extra::text AS extra,
    created_at::text AS created_at, updated_at::text AS updated_at"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_datetime_str_iso8601() {
        let parsed = AccountStore::parse_datetime_str("2026-04-09T12:30:45Z").unwrap();
        let expected = chrono::NaiveDate::from_ymd_opt(2026, 4, 9)
            .unwrap()
            .and_hms_opt(12, 30, 45)
            .unwrap()
            .and_utc();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn test_parse_datetime_str_rfc3339_fractional() {
        let parsed = AccountStore::parse_datetime_str("2026-04-09T12:30:45.123456+00:00").unwrap();
        assert_eq!(parsed.timestamp(), 1775737845);
        assert_eq!(parsed.timestamp_subsec_micros(), 123456);
    }

    #[test]
    fn test_parse_datetime_str_postgres_text_format() {
        let parsed = AccountStore::parse_datetime_str("2026-04-09 12:30:45.123456+00:00").unwrap();
        assert_eq!(parsed.timestamp(), 1775737845);
        assert_eq!(parsed.timestamp_subsec_micros(), 123456);
    }
}
