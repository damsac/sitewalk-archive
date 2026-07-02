//! Signal-driven reflection cadence (spec Rev 3): every session during warmup,
//! exponential backoff while reflections keep changing little, and any user
//! correction snaps cadence back to the next session end.
//!
//! The app layer owns persistence of these counters and calls
//! [`ReflectionPolicy::should_reflect`] whenever it has guaranteed compute
//! (session end / app open). Platform schedulers are out of scope here.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ReflectionSignals {
    /// Sessions completed since the last reflection ran.
    pub sessions_since_reflection: u32,
    /// User corrections (edits of agent output) since the last reflection.
    pub corrections_since_reflection: u32,
    /// Total reflections ever completed (drives warmup).
    pub completed_reflections: u32,
    /// Churn scores of recent reflections, oldest→newest (see ReflectionOutcome::churn).
    pub recent_churn: Vec<f32>,
}

#[derive(Clone, Debug)]
pub struct ReflectionPolicy {
    /// Reflect after every session until this many reflections have run.
    pub warmup_reflections: u32,
    /// Churn below this counts as "nothing new learned".
    pub low_churn_threshold: f32,
    /// Ceiling for the backoff interval.
    pub max_interval_sessions: u32,
}

impl Default for ReflectionPolicy {
    fn default() -> Self {
        ReflectionPolicy {
            warmup_reflections: 5,
            low_churn_threshold: 0.1,
            max_interval_sessions: 16,
        }
    }
}

impl ReflectionPolicy {
    /// Sessions to wait before the next reflection: 1 during warmup or while
    /// churn stays high; doubles per consecutive trailing low-churn reflection.
    pub fn required_interval(&self, completed_reflections: u32, recent_churn: &[f32]) -> u32 {
        if completed_reflections < self.warmup_reflections {
            return 1;
        }
        let trailing_low = recent_churn
            .iter()
            .rev()
            .take_while(|c| **c < self.low_churn_threshold)
            .count() as u32;
        (1u32 << trailing_low.min(6)).min(self.max_interval_sessions)
    }

    pub fn should_reflect(&self, s: &ReflectionSignals) -> bool {
        if s.sessions_since_reflection == 0 {
            return false;
        }
        if s.corrections_since_reflection > 0 {
            return true;
        }
        s.sessions_since_reflection
            >= self.required_interval(s.completed_reflections, &s.recent_churn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals(sessions: u32, corrections: u32, completed: u32, churn: &[f32]) -> ReflectionSignals {
        ReflectionSignals {
            sessions_since_reflection: sessions,
            corrections_since_reflection: corrections,
            completed_reflections: completed,
            recent_churn: churn.to_vec(),
        }
    }

    #[test]
    fn no_new_sessions_means_no_reflection() {
        let p = ReflectionPolicy::default();
        assert!(!p.should_reflect(&signals(0, 0, 0, &[])));
        assert!(!p.should_reflect(&signals(0, 3, 10, &[0.0])), "even corrections wait for a session");
    }

    #[test]
    fn warmup_reflects_every_session() {
        let p = ReflectionPolicy::default(); // warmup_reflections = 5
        assert!(p.should_reflect(&signals(1, 0, 0, &[])));
        assert!(p.should_reflect(&signals(1, 0, 4, &[0.0, 0.0])));
    }

    #[test]
    fn corrections_snap_cadence_back() {
        let p = ReflectionPolicy::default();
        // post-warmup, dead-flat churn, but a correction happened
        assert!(p.should_reflect(&signals(1, 1, 20, &[0.0, 0.0, 0.0])));
    }

    #[test]
    fn low_churn_backs_off_exponentially() {
        let p = ReflectionPolicy::default(); // threshold 0.1, max 16
        // one low-churn reflection → interval 2
        assert!(!p.should_reflect(&signals(1, 0, 6, &[0.05])));
        assert!(p.should_reflect(&signals(2, 0, 6, &[0.05])));
        // three trailing low-churn → interval 8
        assert!(!p.should_reflect(&signals(7, 0, 9, &[0.05, 0.05, 0.05])));
        assert!(p.should_reflect(&signals(8, 0, 9, &[0.05, 0.05, 0.05])));
    }

    #[test]
    fn high_churn_keeps_every_session_cadence() {
        let p = ReflectionPolicy::default();
        assert!(p.should_reflect(&signals(1, 0, 10, &[0.5])));
        // trailing high churn resets the backoff even after earlier low ones
        assert!(p.should_reflect(&signals(1, 0, 10, &[0.05, 0.05, 0.5])));
    }

    #[test]
    fn interval_is_capped_at_max() {
        let p = ReflectionPolicy::default(); // max_interval_sessions = 16
        let flat = [0.0f32; 10];
        assert!(!p.should_reflect(&signals(15, 0, 30, &flat)));
        assert!(p.should_reflect(&signals(16, 0, 30, &flat)));
    }
}
