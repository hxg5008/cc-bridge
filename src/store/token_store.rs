use chrono::{DateTime, Utc};
use sqlx::AnyPool;

use crate::error::AppError;
use crate::model::api_token::ApiToken;

pub struct TokenStore {
    pool: AnyPool,
    /// `get_by_token` 的 LRU 缓存: token 字符串 → (ApiToken, 缓存时间)。
    /// 高 RPS 下 100% 请求都打 DB 查 token 是无意义的浪费, 缓存 30s 既能挡住
    /// 99% 的 SELECT 又能让 admin 改 token / 禁用 token 在 30s 内生效。
    /// 每次 create/update/delete 主动 invalidate 整个缓存以保证强一致 (token 数量
    /// 通常 ≤ 几百, 整表清掉成本极低)。
    cache: dashmap::DashMap<String, (ApiToken, std::time::Instant)>,
}

/// Token 缓存 TTL: 30 秒。短到能让 admin 改 token 几乎实时生效, 长到能挡掉
/// 大部分热路径 SELECT。
const TOKEN_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);
/// 缓存硬上限 (修 N11): 防恶意大量随机 token 把缓存撑爆。
/// 商用 200-300 DAU 总 token 数通常 < 1000, 上限 10K 留 10x 余量。
const TOKEN_CACHE_HARD_CAP: usize = 10_000;

const TOKEN_COLS_PG_TEXT: &str =
    "id, name, token, allowed_accounts, blocked_accounts, status, created_at::text AS created_at, updated_at::text AS updated_at";

impl TokenStore {
    pub fn new(pool: AnyPool) -> Self {
        Self {
            pool,
            cache: dashmap::DashMap::new(),
        }
    }

    /// 主动清空整个 token 缓存 (admin 改 token 后调用, 保证下次 get_by_token 命中新数据)。
    fn invalidate_cache(&self) {
        self.cache.clear();
    }

    fn now_expr(&self) -> &str {
        "NOW()"
    }

    fn select_token_cols(&self) -> &'static str {
        TOKEN_COLS_PG_TEXT
    }

    fn fmt_time(&self, t: DateTime<Utc>) -> String {
        t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
    }

    fn row_to_token(&self, row: &sqlx::any::AnyRow) -> ApiToken {
        use sqlx::Row;
        let parse_time = |col: &str| -> DateTime<Utc> {
            if let Ok(s) = row.try_get::<String, _>(col) {
                s.parse().unwrap_or_else(|_| Utc::now())
            } else {
                Utc::now()
            }
        };
        ApiToken {
            id: row.try_get::<i64, _>("id").unwrap_or_default(),
            name: row.try_get::<String, _>("name").unwrap_or_default(),
            token: row.try_get::<String, _>("token").unwrap_or_default(),
            allowed_accounts: row
                .try_get::<String, _>("allowed_accounts")
                .unwrap_or_default(),
            blocked_accounts: row
                .try_get::<String, _>("blocked_accounts")
                .unwrap_or_default(),
            status: row
                .try_get::<String, _>("status")
                .unwrap_or_else(|_| "active".into())
                .into(),
            created_at: parse_time("created_at"),
            updated_at: parse_time("updated_at"),
        }
    }

    /// 创建令牌
    pub async fn create(&self, t: &mut ApiToken) -> Result<(), AppError> {
        let q = format!(
            "INSERT INTO api_tokens (name, token, allowed_accounts, blocked_accounts, status, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, {now}, {now})",
            now = self.now_expr()
        );
        let result = sqlx::query(&q)
            .bind(&t.name)
            .bind(&t.token)
            .bind(&t.allowed_accounts)
            .bind(&t.blocked_accounts)
            .bind(t.status.to_string())
            .execute(&self.pool)
            .await?;
        t.id = result.last_insert_id().unwrap_or(0) as i64;
        self.invalidate_cache();
        Ok(())
    }

    /// 更新令牌
    pub async fn update(&self, t: &ApiToken) -> Result<(), AppError> {
        let q = format!(
            "UPDATE api_tokens SET name=$1, allowed_accounts=$2, blocked_accounts=$3, status=$4, updated_at={} WHERE id=$5",
            self.now_expr()
        );
        sqlx::query(&q)
            .bind(&t.name)
            .bind(&t.allowed_accounts)
            .bind(&t.blocked_accounts)
            .bind(t.status.to_string())
            .bind(t.id)
            .execute(&self.pool)
            .await?;
        self.invalidate_cache();
        Ok(())
    }

    /// 删除令牌
    pub async fn delete(&self, id: i64) -> Result<(), AppError> {
        sqlx::query("DELETE FROM api_tokens WHERE id=$1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        self.invalidate_cache();
        Ok(())
    }

    /// 按 ID 查询
    pub async fn get_by_id(&self, id: i64) -> Result<ApiToken, AppError> {
        let q = format!("SELECT {} FROM api_tokens WHERE id=$1", self.select_token_cols());
        let row = sqlx::query(&q)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::NotFound)?;
        Ok(self.row_to_token(&row))
    }

    /// 按 token 值查询活跃令牌 — 带 30s LRU 缓存
    pub async fn get_by_token(&self, token: &str) -> Result<Option<ApiToken>, AppError> {
        // fast path: cache 命中 + 未过期
        if let Some(entry) = self.cache.get(token) {
            let (t, ts) = entry.value();
            if ts.elapsed() < TOKEN_CACHE_TTL {
                return Ok(Some(t.clone()));
            }
            // 过期, 删除让下面去 DB 拉
            drop(entry);
            self.cache.remove(token);
        }
        // slow path: 打 DB
        let q = format!(
            "SELECT {} FROM api_tokens WHERE token=$1 AND status='active'",
            self.select_token_cols()
        );
        let row = sqlx::query(&q)
            .bind(token)
            .fetch_optional(&self.pool)
            .await?;
        let result = row.map(|r| self.row_to_token(&r));
        // 只缓存命中的; miss 不缓存避免被穷举攻击撑爆内存
        if let Some(ref t) = result {
            // 修 N11: 命中硬上限时不再插入, 防恶意大量真 token 撑爆
            if self.cache.len() < TOKEN_CACHE_HARD_CAP {
                self.cache
                    .insert(token.to_string(), (t.clone(), std::time::Instant::now()));
            } else {
                tracing::warn!(
                    "token_store cache hit hard cap ({}), not inserting new entry",
                    TOKEN_CACHE_HARD_CAP
                );
            }
        }
        Ok(result)
    }

    /// 列出所有令牌
    pub async fn list(&self) -> Result<Vec<ApiToken>, AppError> {
        let q = format!(
            "SELECT {} FROM api_tokens ORDER BY created_at DESC",
            self.select_token_cols()
        );
        let rows = sqlx::query(&q).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(|r| self.row_to_token(r)).collect())
    }

    /// 分页列出令牌
    pub async fn list_paged(&self, page: i64, page_size: i64) -> Result<Vec<ApiToken>, AppError> {
        let offset = (page - 1) * page_size;
        let q = format!(
            "SELECT {} FROM api_tokens ORDER BY created_at DESC LIMIT $1 OFFSET $2",
            self.select_token_cols()
        );
        let rows = sqlx::query(&q)
            .bind(page_size)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| self.row_to_token(r)).collect())
    }

    /// 计数
    pub async fn count(&self) -> Result<i64, AppError> {
        let row = sqlx::query("SELECT COUNT(*) as cnt FROM api_tokens")
            .fetch_one(&self.pool)
            .await?;
        use sqlx::Row;
        Ok(row.get::<i64, _>("cnt"))
    }
}
