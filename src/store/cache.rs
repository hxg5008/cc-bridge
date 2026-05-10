use crate::error::AppError;
use std::time::Duration;

#[axum::async_trait]
pub trait CacheStore: Send + Sync {
    async fn get_session_account_id(&self, session_hash: &str) -> Result<Option<i64>, AppError>;
    async fn set_session_account_id(
        &self,
        session_hash: &str,
        account_id: i64,
        ttl: Duration,
    ) -> Result<(), AppError>;
    async fn delete_session(&self, session_hash: &str) -> Result<(), AppError>;
    async fn acquire_slot(&self, key: &str, max: i32, ttl: Duration) -> Result<bool, AppError>;
    async fn release_slot(&self, key: &str);
    /// 读取当前槽位计数（best-effort 快照，供管理界面显示）。
    /// Redis 错误或 key 不存在时返回 0；极端情况下由于 DECR 可能产生负数，调用方需自行 clamp。
    async fn peek_slot(&self, key: &str) -> i64;
    async fn acquire_lock(&self, key: &str, owner: &str, ttl: Duration) -> Result<bool, AppError>;
    async fn release_lock(&self, key: &str, owner: &str);
    /// 健康检查 ping (readyz 端点用)。
    /// in-memory store 永远 Ok; Redis store 真打一次 PING 命令验证连通。
    /// 默认实现返回 Ok 是为了向后兼容。
    async fn ping(&self) -> Result<(), AppError> {
        Ok(())
    }
}
