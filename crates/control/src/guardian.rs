use std::sync::Arc;
use std::time::Duration;

use crate::watchdog;

pub const NO_HARDWARE_EXIT_CODE: u32 = 4;
pub const TERMINATE_EXIT_CODE: u32 = 5;
pub const RESUME_GRACE: Duration = Duration::from_secs(30);
pub const RELEASE_BUDGET: Duration = Duration::from_secs(5);
pub const TERMINATE_WAIT: Duration = Duration::from_secs(2);
pub const EXIT_DRAIN: Duration = Duration::from_millis(500);
pub const POLL: Duration = Duration::from_millis(250);
const HUNG_MARGIN: Duration = Duration::from_secs(2);
const HEARTBEAT_FLAG: u8 = 0x80;
const INTERVAL_UNIT_MS: u128 = 250;
const OWNING: u8 = 0x01;
const NOT_OWNING: u8 = 0x02;
const SUSPENDED: u8 = 0x03;
const RESUMED: u8 = 0x04;
const RELEASED: u8 = 0x05;

pub type SignalSink = Arc<dyn Fn(Signal) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Heartbeat { interval: Duration },
    Owning(bool),
    Suspended,
    Resumed,
    Released,
}

impl Signal {
    pub fn encode(self) -> u8 {
        match self {
            Self::Heartbeat { interval } => {
                let units = interval.as_millis().div_ceil(INTERVAL_UNIT_MS);
                HEARTBEAT_FLAG | u8::try_from(units.clamp(1, 0x7F)).unwrap_or(0x7F)
            }
            Self::Owning(true) => OWNING,
            Self::Owning(false) => NOT_OWNING,
            Self::Suspended => SUSPENDED,
            Self::Resumed => RESUMED,
            Self::Released => RELEASED,
        }
    }

    pub fn decode(byte: u8) -> Option<Self> {
        if byte & HEARTBEAT_FLAG != 0 {
            let units = u64::from(byte & !HEARTBEAT_FLAG).max(1);
            return Some(Self::Heartbeat {
                interval: Duration::from_millis(units * INTERVAL_UNIT_MS as u64),
            });
        }
        match byte {
            OWNING => Some(Self::Owning(true)),
            NOT_OWNING => Some(Self::Owning(false)),
            SUSPENDED => Some(Self::Suspended),
            RESUMED => Some(Self::Resumed),
            RELEASED => Some(Self::Released),
            _ => None,
        }
    }
}

pub fn stale_threshold(interval: Duration) -> Duration {
    watchdog::stall_limit(interval) + watchdog::RELEASE_BUDGET + HUNG_MARGIN
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    ParentExited,
    LinkClosed,
    Hung,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Wait,
    Exit,
    Release(Reason),
}

#[derive(Debug, Clone)]
pub struct Monitor {
    last_heartbeat: Duration,
    interval: Duration,
    owning: bool,
    suspended: bool,
    released: bool,
    link_closed: bool,
    parent_exited_at: Option<Duration>,
    resumed_at: Option<Duration>,
}

impl Monitor {
    pub fn new(now: Duration) -> Self {
        Self {
            last_heartbeat: now,
            interval: Duration::from_secs(1),
            owning: false,
            suspended: false,
            released: false,
            link_closed: false,
            parent_exited_at: None,
            resumed_at: None,
        }
    }

    pub fn observe(&mut self, signal: Signal, now: Duration) {
        match signal {
            Signal::Heartbeat { interval } => {
                self.interval = interval;
                self.last_heartbeat = now;
            }
            Signal::Owning(owning) => self.owning = owning,
            Signal::Suspended => self.suspended = true,
            Signal::Resumed => {
                self.suspended = false;
                self.system_resumed(now);
            }
            Signal::Released => self.released = true,
        }
    }

    pub fn system_resumed(&mut self, now: Duration) {
        self.resumed_at = Some(now);
        self.last_heartbeat = now;
    }

    pub fn link_closed(&mut self) {
        self.link_closed = true;
    }

    pub fn parent_exited(&mut self, now: Duration) {
        self.parent_exited_at.get_or_insert(now);
    }

    fn in_resume_grace(&self, now: Duration) -> bool {
        self.resumed_at
            .is_some_and(|resumed| now.saturating_sub(resumed) < RESUME_GRACE)
    }

    pub fn decide(&self, now: Duration) -> Action {
        if self.released {
            return if self.link_closed || self.parent_exited_at.is_some() {
                Action::Exit
            } else {
                Action::Wait
            };
        }
        if self.link_closed {
            return Action::Release(Reason::LinkClosed);
        }
        if let Some(exited) = self.parent_exited_at {
            return if now.saturating_sub(exited) >= EXIT_DRAIN {
                Action::Release(Reason::ParentExited)
            } else {
                Action::Wait
            };
        }
        let age = now.saturating_sub(self.last_heartbeat);
        let hung = self.owning
            && !self.suspended
            && !self.in_resume_grace(now)
            && age > stale_threshold(self.interval);
        if hung {
            Action::Release(Reason::Hung)
        } else {
            Action::Wait
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkEvent {
    Signal(Signal),
    Closed,
    ParentExited,
    SystemResumed,
    Idle,
}

pub trait ParentLink {
    fn next_event(&mut self, timeout: Duration) -> LinkEvent;
    fn parent_alive(&mut self) -> bool;
    fn terminate_parent(&mut self, wait: Duration);
}

pub trait Releaser {
    fn release(&mut self, budget: Duration) -> Result<(), String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    CleanExit,
    Released(Reason),
    ReleaseFailed(Reason, String),
}

pub fn run(
    link: &mut impl ParentLink,
    releaser: &mut impl Releaser,
    clock: &dyn Fn() -> Duration,
) -> Outcome {
    let mut monitor = Monitor::new(clock());
    loop {
        match link.next_event(POLL) {
            LinkEvent::Signal(signal) => monitor.observe(signal, clock()),
            LinkEvent::Closed => monitor.link_closed(),
            LinkEvent::ParentExited => monitor.parent_exited(clock()),
            LinkEvent::SystemResumed => monitor.system_resumed(clock()),
            LinkEvent::Idle => {}
        }
        match monitor.decide(clock()) {
            Action::Wait => {}
            Action::Exit => return Outcome::CleanExit,
            Action::Release(reason) => {
                tracing::error!(
                    ?reason,
                    "FanForge stopped without releasing the fans; returning them to BIOS control"
                );
                if link.parent_alive() {
                    link.terminate_parent(TERMINATE_WAIT);
                }
                return match releaser.release(RELEASE_BUDGET) {
                    Ok(()) => Outcome::Released(reason),
                    Err(error) => Outcome::ReleaseFailed(reason, error),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use fanforge_hw::ite::It8655e;
    use fanforge_hw::ite::fake::{FakeIte, SharedFake};
    use fanforge_hw::mutex::NamedMutex;

    use super::*;
    use crate::failsafe;

    const SECOND: Duration = Duration::from_secs(1);

    fn owning_monitor() -> Monitor {
        let mut monitor = Monitor::new(Duration::ZERO);
        monitor.observe(Signal::Heartbeat { interval: SECOND }, Duration::ZERO);
        monitor.observe(Signal::Owning(true), Duration::ZERO);
        monitor
    }

    #[test]
    fn signals_round_trip_through_one_byte() {
        let signals = [
            Signal::Heartbeat {
                interval: Duration::from_millis(250),
            },
            Signal::Heartbeat {
                interval: Duration::from_secs(5),
            },
            Signal::Owning(true),
            Signal::Owning(false),
            Signal::Suspended,
            Signal::Resumed,
            Signal::Released,
        ];
        for signal in signals {
            assert_eq!(Signal::decode(signal.encode()), Some(signal));
        }
        assert_eq!(Signal::decode(0x00), None);
        assert_eq!(Signal::decode(0x7F), None);
    }

    #[test]
    fn the_guardian_always_waits_longer_than_the_watchdog() {
        for millis in [250, 1000, 2000, 5000] {
            let interval = Duration::from_millis(millis);
            assert!(
                stale_threshold(interval)
                    > watchdog::stall_limit(interval) + watchdog::RELEASE_BUDGET
            );
        }
        assert_eq!(stale_threshold(SECOND), Duration::from_secs(6));
    }

    #[test]
    fn a_fresh_heartbeat_waits_and_a_stale_one_releases() {
        let monitor = owning_monitor();
        assert_eq!(monitor.decide(Duration::from_secs(6)), Action::Wait);
        assert_eq!(
            monitor.decide(Duration::from_millis(6001)),
            Action::Release(Reason::Hung)
        );
    }

    #[test]
    fn stale_heartbeats_are_ignored_unless_owning() {
        let mut monitor = owning_monitor();
        monitor.observe(Signal::Owning(false), Duration::ZERO);
        assert_eq!(monitor.decide(Duration::from_secs(60)), Action::Wait);
    }

    #[test]
    fn suspension_and_resume_grace_hold_off_the_hang_check() {
        let mut monitor = owning_monitor();
        monitor.observe(Signal::Suspended, Duration::ZERO);
        assert_eq!(monitor.decide(Duration::from_secs(600)), Action::Wait);
        monitor.observe(Signal::Resumed, Duration::from_secs(600));
        assert_eq!(monitor.decide(Duration::from_secs(629)), Action::Wait);
        assert_eq!(
            monitor.decide(Duration::from_secs(631)),
            Action::Release(Reason::Hung)
        );

        let mut monitor = owning_monitor();
        monitor.system_resumed(Duration::from_secs(10));
        assert_eq!(monitor.decide(Duration::from_secs(39)), Action::Wait);
    }

    #[test]
    fn end_of_file_releases_unless_released_was_sent() {
        let mut monitor = owning_monitor();
        monitor.link_closed();
        assert_eq!(monitor.decide(SECOND), Action::Release(Reason::LinkClosed));

        let mut monitor = owning_monitor();
        monitor.observe(Signal::Released, SECOND);
        assert_eq!(monitor.decide(Duration::from_secs(100)), Action::Wait);
        monitor.link_closed();
        assert_eq!(monitor.decide(Duration::from_secs(100)), Action::Exit);
    }

    #[test]
    fn a_parent_exit_waits_briefly_for_a_late_released() {
        let mut monitor = owning_monitor();
        monitor.parent_exited(SECOND);
        assert_eq!(monitor.decide(SECOND), Action::Wait);
        assert_eq!(
            monitor.decide(SECOND + EXIT_DRAIN),
            Action::Release(Reason::ParentExited)
        );
        monitor.observe(Signal::Released, SECOND);
        assert_eq!(monitor.decide(SECOND + EXIT_DRAIN), Action::Exit);
    }

    struct FakeLink {
        clock: Rc<Cell<Duration>>,
        script: VecDeque<(Duration, LinkEvent)>,
        alive: bool,
        terminated: bool,
    }

    impl ParentLink for FakeLink {
        fn next_event(&mut self, timeout: Duration) -> LinkEvent {
            let now = self.clock.get();
            match self.script.front() {
                Some((at, _)) if *at <= now + timeout => {
                    let (at, event) = self.script.pop_front().unwrap_or((now, LinkEvent::Idle));
                    self.clock.set(at.max(now));
                    if event == LinkEvent::ParentExited {
                        self.alive = false;
                    }
                    event
                }
                _ => {
                    self.clock.set(now + timeout);
                    LinkEvent::Idle
                }
            }
        }

        fn parent_alive(&mut self) -> bool {
            self.alive
        }

        fn terminate_parent(&mut self, _wait: Duration) {
            self.terminated = true;
            self.alive = false;
        }
    }

    struct FakeReleaser {
        chip: It8655e<SharedFake>,
        snapshot: tempfile::TempDir,
    }

    impl Releaser for FakeReleaser {
        fn release(&mut self, budget: Duration) -> Result<(), String> {
            failsafe::release_with(
                &mut self.chip,
                &self.snapshot.path().join("snapshot.toml"),
                budget,
            )
            .map_err(|error| error.to_string())
        }
    }

    fn rig(tag: &str, script: Vec<(Duration, LinkEvent)>) -> (FakeLink, FakeReleaser, SharedFake) {
        let fake = SharedFake::new(FakeIte::new());
        fake.with(|chip| chip.banks[0][0x16] = 0x00);
        let lock = NamedMutex::open(&format!(
            r"Local\fanforge-guardian-{tag}-{}",
            std::process::id()
        ))
        .unwrap();
        let releaser = FakeReleaser {
            chip: It8655e::probe(fake.clone(), lock).unwrap(),
            snapshot: tempfile::tempdir().unwrap(),
        };
        let link = FakeLink {
            clock: Rc::new(Cell::new(Duration::ZERO)),
            script: script.into(),
            alive: true,
            terminated: false,
        };
        (link, releaser, fake)
    }

    fn beats(until: Duration) -> Vec<(Duration, LinkEvent)> {
        let mut script = vec![(Duration::ZERO, LinkEvent::Signal(Signal::Owning(true)))];
        let mut at = Duration::ZERO;
        while at <= until {
            script.push((
                at,
                LinkEvent::Signal(Signal::Heartbeat { interval: SECOND }),
            ));
            at += SECOND;
        }
        script
    }

    #[test]
    fn a_killed_parent_has_its_fans_released() {
        let mut script = beats(Duration::from_secs(3));
        script.push((Duration::from_millis(3500), LinkEvent::ParentExited));
        let (mut link, mut releaser, fake) = rig("killed", script);
        let clock = Rc::clone(&link.clock);
        let outcome = run(&mut link, &mut releaser, &move || clock.get());
        assert_eq!(outcome, Outcome::Released(Reason::ParentExited));
        assert!(!link.terminated);
        assert_eq!(fake.with(|chip| chip.reg(0x16)) & 0x80, 0x80);
    }

    #[test]
    fn a_hung_parent_is_terminated_then_released() {
        let (mut link, mut releaser, fake) = rig("hung", beats(Duration::from_secs(2)));
        let clock = Rc::clone(&link.clock);
        let outcome = run(&mut link, &mut releaser, &move || clock.get());
        assert_eq!(outcome, Outcome::Released(Reason::Hung));
        assert!(link.terminated);
        assert!(link.clock.get() > Duration::from_secs(8));
        assert_eq!(fake.with(|chip| chip.reg(0x16)) & 0x80, 0x80);
    }

    #[test]
    fn a_clean_exit_leaves_the_fans_alone() {
        let mut script = beats(Duration::from_secs(2));
        script.push((
            Duration::from_millis(2500),
            LinkEvent::Signal(Signal::Released),
        ));
        script.push((Duration::from_millis(2600), LinkEvent::ParentExited));
        script.push((Duration::from_millis(2700), LinkEvent::Closed));
        let (mut link, mut releaser, fake) = rig("clean", script);
        let clock = Rc::clone(&link.clock);
        let outcome = run(&mut link, &mut releaser, &move || clock.get());
        assert_eq!(outcome, Outcome::CleanExit);
        assert_eq!(fake.with(|chip| chip.reg(0x16)), 0x00);
    }
}
