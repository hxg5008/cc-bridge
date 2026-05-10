use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use crate::error::AppError;
use crate::store::cache::CacheStore;

/// 全局上限: 防止 in-memory 模式被脏会话哈希撑爆 RAM。
/// 命中上限时按"最先过期"批量淘汰 1/4。
const MEMORY_SESSIONS_HARD_CAP: usize = 200_000;
const MEMORY_LOCKS_HARD_CAP: usize = 50_000;

/// 后台 GC 周期。
const GC_INTERVAL: Duration = Duration::from_secs(60);

struct SessionEntry {
    account_id: i64,
    expires_at: tokio::time::Instant,
}

struct LockEntry {
    owner: String,
    expires_at: tokio::time::Instant,
}

pub struct MemoryStore {
    sessions: Mutex<HashMap<String, SessionEntry>>,
    slots: Mutex<HashMap<String, i64>>,
    locks: Mutex<HashMap<String, LockEntry>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            slots: Mutex::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
        }
    }

    /// 启动后台 GC: 每 60s 扫一次, 删过期 session/lock; 命中硬上限时批量淘汰
    /// 旧条目。slots 不需要 GC: 它由 acquire/release 自然成对维护, 长跑账号
    /// 不会增长 (Redis 模式有 TTL, 这里 in-memory 模式 acquire_slot 的 _ttl
    /// 参数被忽略, 由 release_slot 维持平衡; 删账号时上层 service 应清掉)。
    ///
    /// **必须由持有者 (CacheStore Arc) 在启动时调一次**, 内部 spawn detached
    /// task, 进程结束自然回收。
    pub fn spawn_gc(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(GC_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(this) = weak.upgrade() else { return; };
                this.gc_once().await;
            }
        });
    }

    async fn gc_once(&self) {
        let now = tokio::time::Instant::now();
        // sessions
        {
            let mut sessions = self.sessions.lock().await;
            let before = sessions.len();
            sessions.retain(|_, e| e.expires_at > now);
            // 命中硬上限 → 按到期时间升序淘汰 1/4
            if sessions.len() > MEMORY_SESSIONS_HARD_CAP {
                let mut v: Vec<(String, tokio::time::Instant)> =
                    sessions.iter().map(|(k, e)| (k.clone(), e.expires_at)).collect();
                v.sort_by_key(|x| x.1);
                let evict = sessions.len() / 4;
                for (k, _) in v.into_iter().take(evict) {
                    sessions.remove(&k);
                }
            }
            let after = sessions.len();
            if before != after {
                tracing::debug!(before, after, "memory_store gc sessions");
            }
        }
        // locks
        {
            let mut locks = self.locks.lock().await;
            let before = locks.len();
            locks.retain(|_, e| e.expires_at > now);
            if locks.len() > MEMORY_LOCKS_HARD_CAP {
                let mut v: Vec<(String, tokio::time::Instant)> =
                    locks.iter().map(|(k, e)| (k.clone(), e.expires_at)).collect();
                v.sort_by_key(|x| x.1);
                let evict = locks.len() / 4;
                for (k, _) in v.into_iter().take(evict) {
                    locks.remove(&k);
                }
            }
            let after = locks.len();
            if before != after {
                tracing::debug!(before, after, "memory_store gc locks");
            }
        }
    }
}

#[axum::async_trait]
impl CacheStore for MemoryStore {
    async fn get_session_account_id(&self, session_hash: &str) -> Result<Option<i64>, AppError> {
        let mut sessions = self.sessions.lock().await;
        let key = format!("session:{}", session_hash);
        if let Some(entry) = sessions.get(&key) {
            if tokio::time::Instant::now() > entry.expires_at {
                sessions.remove(&key);
                return Ok(None);
            }
            return Ok(Some(entry.account_id));
        }
        Ok(None)
    }

    async fn set_session_account_id(
        &self,
        session_hash: &str,
        account_id: i64,
        ttl: Duration,
    ) -> Result<(), AppError> {
        let mut sessions = self.sessions.lock().await;
        let key = format!("session:{}", session_hash);
        sessions.insert(
            key,
            SessionEntry {
                account_id,
                expires_at: tokio::time::Instant::now() + ttl,
            },
        );
        Ok(())
    }

    async fn delete_session(&self, session_hash: &str) -> Result<(), AppError> {
        let mut sessions = self.sessions.lock().await;
        sessions.remove(&format!("session:{}", session_hash));
        Ok(())
    }

    async fn acquire_slot(&self, key: &str, max: i32, _ttl: Duration) -> Result<bool, AppError> {
        let mut slots = self.slots.lock().await;
        let val = slots.entry(key.to_string()).or_insert(0);
        *val += 1;
        if *val > max as i64 {
            *val -= 1;
            return Ok(false);
        }
        Ok(true)
    }

    async fn release_slot(&self, key: &str) {
        let mut slots = self.slots.lock().await;
        if let Some(val) = slots.get_mut(key) {
            if *val > 0 {
                *val -= 1;
            }
        }
    }

    async fn peek_slot(&self, key: &str) -> i64 {
        self.slots.lock().await.get(key).copied().unwrap_or(0)
    }

    async fn acquire_lock(&self, key: &str, owner: &str, ttl: Duration) -> Result<bool, AppError> {
        let mut locks = self.locks.lock().await;
        let now = tokio::time::Instant::now();
        if let Some(existing) = locks.get(key) {
            if now <= existing.expires_at {
                return Ok(false);
            }
        }
        locks.insert(
            key.to_string(),
            LockEntry {
                owner: owner.to_string(),
                expires_at: now + ttl,
            },
        );
        Ok(true)
    }

    async fn release_lock(&self, key: &str, owner: &str) {
        let mut locks = self.locks.lock().await;
        if let Some(existing) = locks.get(key) {
            if existing.owner == owner {
                locks.remove(key);
            }
        }
    }
}
