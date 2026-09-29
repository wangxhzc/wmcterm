use std::collections::HashSet;

/// Ordered hook invocations distinguish a fresh resume from a delayed replay.
#[derive(Clone, Default)]
pub(super) struct SessionStartGuard {
    current: Option<String>,
    seen: HashSet<String>,
    newest: Option<u64>,
}

impl SessionStartGuard {
    pub(super) fn observe(&mut self, session: &str, emitted: Option<u64>) -> Result<bool, String> {
        if let (Some(emitted), Some(newest)) = (emitted, self.newest) {
            if emitted < newest {
                return Err("stale SessionStart delivery".into());
            }
            if emitted == newest {
                return if self.current.as_deref() == Some(session) {
                    Ok(false)
                } else {
                    Err("ambiguous SessionStart delivery".into())
                };
            }
        }
        if self.current.as_deref() != Some(session)
            && self.seen.contains(session)
            && emitted.is_none()
        {
            return Err("retired session requires a fresh timestamped SessionStart".into());
        }
        if !self.seen.contains(session) && self.seen.len() >= 4096 {
            return Err("session history limit reached; restart the terminal".into());
        }
        self.seen.insert(session.into());
        self.current = Some(session.into());
        if emitted.is_some() {
            self.newest = emitted;
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_replay_but_accepts_new_resume() {
        let mut guard = SessionStartGuard::default();
        assert!(guard.observe("a", Some(10)).unwrap());
        assert!(guard.observe("b", Some(20)).unwrap());
        assert!(guard.observe("a", Some(10)).is_err());
        assert!(guard.observe("a", None).is_err());
        assert!(guard.observe("a", Some(30)).unwrap());
        assert!(!guard.observe("a", Some(30)).unwrap());
        assert!(guard.observe("b", Some(30)).is_err());
    }
}
