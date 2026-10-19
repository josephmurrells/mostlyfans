use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use fanforge_core::FanId;
use fanforge_core::config::Config;
use fanforge_core::state::ControlState;

pub enum LoopMessage {
    ReplaceConfig(Box<Config>, SyncSender<Result<(), String>>),
    SetActiveProfile(String, SyncSender<Result<(), String>>),
    Identify(FanId),
    Suspend(SyncSender<()>),
    Resume,
    RetryHardware,
    Stop,
}

#[derive(Debug)]
pub struct Heartbeat {
    origin: Instant,
    last_beat_ms: AtomicU64,
    interval_ms: AtomicU64,
    paused: AtomicBool,
    hardware_ready: AtomicBool,
}

impl Heartbeat {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
            last_beat_ms: AtomicU64::new(0),
            interval_ms: AtomicU64::new(1000),
            paused: AtomicBool::new(true),
            hardware_ready: AtomicBool::new(false),
        }
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    pub fn beat(&self, interval: Duration, hardware_ready: bool) {
        self.interval_ms.store(
            u64::try_from(interval.as_millis()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        self.hardware_ready.store(hardware_ready, Ordering::Relaxed);
        self.last_beat_ms.store(self.now_ms(), Ordering::Release);
        self.paused.store(false, Ordering::Release);
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::Release);
    }

    pub fn stalled_for(&self, minimum: Duration) -> Option<Duration> {
        if self.paused.load(Ordering::Acquire) || !self.hardware_ready.load(Ordering::Relaxed) {
            return None;
        }
        let limit =
            Duration::from_millis(self.interval_ms.load(Ordering::Relaxed) * 3).max(minimum);
        let age = Duration::from_millis(
            self.now_ms()
                .saturating_sub(self.last_beat_ms.load(Ordering::Acquire)),
        );
        (age > limit).then_some(age)
    }
}

#[derive(Debug)]
pub struct Shared {
    pub state: RwLock<Option<Arc<ControlState>>>,
    pub heartbeat: Heartbeat,
    halted: AtomicBool,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            state: RwLock::new(None),
            heartbeat: Heartbeat::new(),
            halted: AtomicBool::new(false),
        }
    }
}

impl Shared {
    pub fn halt(&self) {
        self.halted.store(true, Ordering::SeqCst);
    }

    pub fn is_halted(&self) -> bool {
        self.halted.load(Ordering::SeqCst)
    }

    pub fn halt_flag(&self) -> &AtomicBool {
        &self.halted
    }

    pub fn state(&self) -> Option<Arc<ControlState>> {
        self.state.read().ok().and_then(|state| state.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_reports_stalls_only_when_active() {
        let heartbeat = Heartbeat::new();
        assert_eq!(heartbeat.stalled_for(Duration::ZERO), None);
        heartbeat.beat(Duration::from_millis(1), true);
        std::thread::sleep(Duration::from_millis(20));
        assert!(heartbeat.stalled_for(Duration::ZERO).is_some());
        assert_eq!(heartbeat.stalled_for(Duration::from_secs(60)), None);
        heartbeat.pause();
        assert_eq!(heartbeat.stalled_for(Duration::ZERO), None);
        heartbeat.beat(Duration::from_millis(1), false);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(heartbeat.stalled_for(Duration::ZERO), None);
    }

    #[test]
    fn halting_is_sticky() {
        let shared = Shared::default();
        assert!(!shared.is_halted());
        shared.halt();
        shared.halt();
        assert!(shared.is_halted());
    }
}
