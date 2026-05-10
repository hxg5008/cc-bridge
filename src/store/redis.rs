use redis::AsyncCommands;
use std::time::Duration;

use crate::error::AppError;
use crate::store::cache::CacheStore;

pub struct RedisStore {
    client: redis::aio::ConnectionManager,
}

/// Build a Redis connection URL from discrete components.
///
/// Kept as a pure function so it can be unit-tested without a live Redis server.
/// The database number is encoded exactly once as the URL path.
pub fn build_redis_url(host: &str, port: u16, password: &str, db: i64) -> String {
    if password.is_empty() {
        format!("redis://{}:{}/{}", host, port, db)
    } else {
        format!("redis://:{}@{}:{}/{}", password, host, port, db)
    }
}

impl RedisStore {
    pub async fn new(host: &str, port: u16, password: &str, db: i64) -> Result<Self, AppError> {
        let url = build_redis_url(host, port, password, db);
        let client = redis::Client::open(url)
            .map_err(|e| AppError::Internal(format!("redis open: {}", e)))?;
        let mgr = redis::aio::ConnectionManager::new(client)
            .await
            .map_err(|e| AppError::Internal(format!("redis connect: {}", e)))?;
        Ok(Self { client: mgr })
    }
}

#[axum::async_trait]
impl CacheStore for RedisStore {
    async fn get_session_account_id(&self, session_hash: &str) -> Result<Option<i64>, AppError> {
        let key = format!("session:{}", session_hash);
        let val: Option<String> = self
            .client
            .clone()
            .get(&key)
            .await
            .map_err(|e| AppError::Internal(format!("redis get: {}", e)))?;
        match val {
            Some(s) => {
                let id = s
                    .parse::<i64>()
                    .map_err(|e| AppError::Internal(format!("redis parse: {}", e)))?;
                Ok(Some(id))
            }
            None => Ok(None),
        }
    }

    async fn set_session_account_id(
        &self,
        session_hash: &str,
        account_id: i64,
        ttl: Duration,
    ) -> Result<(), AppError> {
        let key = format!("session:{}", session_hash);
        let _: () = self
            .client
            .clone()
            .set_ex(&key, account_id.to_string(), ttl.as_secs())
            .await
            .map_err(|e| AppError::Internal(format!("redis set: {}", e)))?;
        Ok(())
    }

    async fn delete_session(&self, session_hash: &str) -> Result<(), AppError> {
        let key = format!("session:{}", session_hash);
        let _: () = self
            .client
            .clone()
            .del(&key)
            .await
            .map_err(|e| AppError::Internal(format!("redis del: {}", e)))?;
        Ok(())
    }

    async fn acquire_slot(&self, key: &str, max: i32, ttl: Duration) -> Result<bool, AppError> {
        // Lua 脚本原子完成: INCR + (val=1 ? EXPIRE) + 超 max 时 DECR 回滚
        // 旧实现 INCR 和 EXPIRE 分两次 RTT, 中间网络抖动 EXPIRE 丢包
        // 会造成 key 永久存活 → 该账号并发槽永久占用一个名额, 累积到 max
        // 后整账号被永久判为"满载"无法调度。
        //
        // 返回: 1 = 拿到槽; 0 = 已满
        let script = redis::Script::new(
            r#"
            local v = redis.call("INCR", KEYS[1])
            if v == 1 then
                redis.call("EXPIRE", KEYS[1], ARGV[2])
            end
            if v > tonumber(ARGV[1]) then
                redis.call("DECR", KEYS[1])
                return 0
            end
            return 1
            "#,
        );
        let mut conn = self.client.clone();
        let acquired: i64 = script
            .key(key)
            .arg(max as i64)
            .arg(ttl.as_secs().max(1) as i64)
            .invoke_async(&mut conn)
            .await
            .map_err(|e| AppError::Internal(format!("redis acquire_slot: {}", e)))?;
        Ok(acquired == 1)
    }

    async fn release_slot(&self, key: &str) {
        // 不再 unwrap_or 静默吞错, 失败要可观测 (错过 release 累积会让槽失效)
        if let Err(e) = self.client.clone().decr::<_, _, i64>(key, 1i64).await {
            tracing::warn!(key = %key, error = %e, "redis release_slot failed (slot may leak until TTL)");
        }
    }

    async fn peek_slot(&self, key: &str) -> i64 {
        match self.client.clone().get::<_, Option<String>>(key).await {
            Ok(Some(s)) => s.parse::<i64>().unwrap_or(0),
            Ok(None) => 0,
            Err(e) => {
                tracing::warn!(key = %key, error = %e, "redis peek_slot failed, returning 0");
                0
            }
        }
    }

    async fn acquire_lock(&self, key: &str, owner: &str, ttl: Duration) -> Result<bool, AppError> {
        let mut conn = self.client.clone();
        let result: Option<String> = redis::cmd("SET")
            .arg(key)
            .arg(owner)
            .arg("NX")
            .arg("EX")
            .arg(ttl.as_secs().max(1))
            .query_async(&mut conn)
            .await
            .map_err(|e| AppError::Internal(format!("redis lock set: {}", e)))?;
        Ok(result.is_some())
    }

    async fn release_lock(&self, key: &str, owner: &str) {
        let mut conn = self.client.clone();
        let script = redis::Script::new(
            r#"
            if redis.call("GET", KEYS[1]) == ARGV[1] then
                return redis.call("DEL", KEYS[1])
            end
            return 0
            "#,
        );
        let _: Result<i32, _> = script.key(key).arg(owner).invoke_async(&mut conn).await;
    }

    async fn ping(&self) -> Result<(), AppError> {
        let mut conn = self.client.clone();
        let _: String = redis::cmd("PING")
            .query_async(&mut conn)
            .await
            .map_err(|e| AppError::Internal(format!("redis ping: {}", e)))?;
        Ok(())
    }
}
