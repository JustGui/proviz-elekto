use dashmap::DashMap;
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::models::RateLimitErrorType;

/// In-memory rate limit state. Thread-safe, O(1) check.
/// Stores (expiry Instant) per model_id.
pub struct RateLimitState {
    limited: DashMap<Uuid, Instant>,
}

impl Default for RateLimitState {
    fn default() -> Self {
        Self {
            limited: DashMap::new(),
        }
    }
}

impl RateLimitState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mark(&self, model_id: Uuid, error_type: &RateLimitErrorType) {
        let ttl = error_type.cooldown_secs();
        if ttl == 0 {
            return; // parse failures: don't block model, just log
        }
        let expiry = Instant::now() + Duration::from_secs(ttl);
        self.limited.insert(model_id, expiry);
    }

    pub fn is_limited(&self, model_id: &Uuid) -> bool {
        match self.limited.get(model_id) {
            None => false,
            Some(expiry) => {
                if Instant::now() < *expiry {
                    true
                } else {
                    drop(expiry);
                    self.limited.remove(model_id);
                    false
                }
            }
        }
    }

    pub fn clear(&self, model_id: &Uuid) {
        self.limited.remove(model_id);
    }

    #[cfg(test)]
    pub fn insert_expired(&self, id: Uuid) {
        self.limited
            .insert(id, Instant::now() - Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_tpm_blocks() {
        let s = RateLimitState::new();
        let id = Uuid::new_v4();
        s.mark(id, &RateLimitErrorType::Tpm);
        assert!(s.is_limited(&id));
    }

    #[test]
    fn mark_rpm_blocks() {
        let s = RateLimitState::new();
        let id = Uuid::new_v4();
        s.mark(id, &RateLimitErrorType::Rpm);
        assert!(s.is_limited(&id));
    }

    #[test]
    fn mark_tpd_blocks() {
        let s = RateLimitState::new();
        let id = Uuid::new_v4();
        s.mark(id, &RateLimitErrorType::Tpd);
        assert!(s.is_limited(&id));
    }

    #[test]
    fn mark_auth_blocks() {
        let s = RateLimitState::new();
        let id = Uuid::new_v4();
        s.mark(id, &RateLimitErrorType::Auth);
        assert!(s.is_limited(&id));
    }

    #[test]
    fn mark_timeout_blocks() {
        let s = RateLimitState::new();
        let id = Uuid::new_v4();
        s.mark(id, &RateLimitErrorType::Timeout);
        assert!(s.is_limited(&id));
    }

    #[test]
    fn mark_other_blocks() {
        let s = RateLimitState::new();
        let id = Uuid::new_v4();
        s.mark(id, &RateLimitErrorType::Other);
        assert!(s.is_limited(&id));
    }

    #[test]
    fn parse_does_not_block() {
        let s = RateLimitState::new();
        let id = Uuid::new_v4();
        s.mark(id, &RateLimitErrorType::Parse);
        assert!(!s.is_limited(&id));
    }

    #[test]
    fn clear_removes_limit() {
        let s = RateLimitState::new();
        let id = Uuid::new_v4();
        s.mark(id, &RateLimitErrorType::Tpm);
        assert!(s.is_limited(&id));
        s.clear(&id);
        assert!(!s.is_limited(&id));
    }

    #[test]
    fn unknown_id_not_limited() {
        let s = RateLimitState::new();
        let id = Uuid::new_v4();
        assert!(!s.is_limited(&id));
    }

    #[test]
    fn clear_unknown_noop() {
        let s = RateLimitState::new();
        s.clear(&Uuid::new_v4()); // must not panic
    }

    #[test]
    fn models_independent() {
        let s = RateLimitState::new();
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        s.mark(id_a, &RateLimitErrorType::Tpm);
        assert!(s.is_limited(&id_a));
        assert!(!s.is_limited(&id_b));
    }

    #[test]
    fn expired_entry_not_limited() {
        let s = RateLimitState::new();
        let id = Uuid::new_v4();
        s.insert_expired(id);
        assert!(!s.is_limited(&id));
    }
}
