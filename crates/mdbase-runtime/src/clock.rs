use std::sync::{Arc, RwLock};

use chrono::{DateTime, Duration, Utc};

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[derive(Debug, Clone)]
pub struct ManualClock {
    now: Arc<RwLock<DateTime<Utc>>>,
}

impl ManualClock {
    pub fn new(now: DateTime<Utc>) -> Self {
        Self {
            now: Arc::new(RwLock::new(now)),
        }
    }

    pub fn set(&self, now: DateTime<Utc>) {
        *self.now.write().expect("manual clock lock poisoned") = now;
    }

    pub fn advance(&self, duration: Duration) {
        let mut now = self.now.write().expect("manual clock lock poisoned");
        *now += duration;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.now.read().expect("manual clock lock poisoned")
    }
}
