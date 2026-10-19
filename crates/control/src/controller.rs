use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use fanforge_core::FanId;
use fanforge_core::config::Config;
use fanforge_core::state::ControlState;

use crate::backend::Backend;
use crate::runtime::Runtime;
use crate::shared::{LoopMessage, Shared};

pub const CONTROL_THREAD: &str = "control";
pub const QUIT_WAIT: Duration = Duration::from_secs(8);
pub const DIRECT_RELEASE_BUDGET: Duration = Duration::from_secs(3);
pub const SUSPEND_ACK_TIMEOUT: Duration = Duration::from_secs(2);
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
const NOT_RUNNING: &str = "fan control is not running";

pub type RuntimeFactory<B> =
    Box<dyn FnMut(Arc<Shared>, Receiver<LoopMessage>) -> Runtime<B> + Send>;
pub type DirectRelease = Arc<dyn Fn(Duration) -> bool + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlStatus {
    Running,
    Stopped(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestKind {
    Config,
    Profile,
    Identify,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandResult {
    pub kind: RequestKind,
    pub outcome: Result<(), String>,
}

#[derive(Debug)]
pub enum Request {
    ReplaceConfig(Box<Config>),
    SetActiveProfile(String),
    Identify(FanId),
}

impl Request {
    fn kind(&self) -> RequestKind {
        match self {
            Self::ReplaceConfig(_) => RequestKind::Config,
            Self::SetActiveProfile(_) => RequestKind::Profile,
            Self::Identify(_) => RequestKind::Identify,
        }
    }
}

#[derive(Debug)]
enum Exit {
    Finished { released: bool },
    Panicked,
}

struct Pending {
    kind: RequestKind,
    reply: Receiver<Result<(), String>>,
    sent: Instant,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct Remote {
    sender: Arc<Mutex<Sender<LoopMessage>>>,
    shared: Arc<Shared>,
}

impl Remote {
    pub fn suspend(&self) -> bool {
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        lock(&self.sender)
            .send(LoopMessage::Suspend(done_tx))
            .is_ok()
            && done_rx.recv_timeout(SUSPEND_ACK_TIMEOUT).is_ok()
    }

    pub fn resume(&self) {
        lock(&self.sender).send(LoopMessage::Resume).ok();
    }

    pub fn halt(&self) {
        self.shared.halt();
    }

    pub fn retry_hardware(&self) {
        lock(&self.sender).send(LoopMessage::RetryHardware).ok();
    }
}

pub struct Controller<B: Backend + Send + 'static> {
    shared: Arc<Shared>,
    factory: RuntimeFactory<B>,
    sender: Arc<Mutex<Sender<LoopMessage>>>,
    exit: Option<Receiver<Exit>>,
    status: Arc<Mutex<ControlStatus>>,
    pending: Mutex<Vec<Pending>>,
    results: Mutex<Vec<CommandResult>>,
    direct_release: DirectRelease,
    on_released: Option<Box<dyn FnOnce() + Send>>,
    finished: Option<bool>,
}

impl<B: Backend + Send + 'static> Controller<B> {
    pub fn start(
        shared: Arc<Shared>,
        factory: RuntimeFactory<B>,
        direct_release: DirectRelease,
    ) -> io::Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let mut controller = Self {
            shared,
            factory,
            sender: Arc::new(Mutex::new(sender)),
            exit: None,
            status: Arc::new(Mutex::new(ControlStatus::Running)),
            pending: Mutex::new(Vec::new()),
            results: Mutex::new(Vec::new()),
            direct_release,
            on_released: None,
            finished: None,
        };
        controller.launch(receiver)?;
        Ok(controller)
    }

    pub fn on_released(&mut self, callback: impl FnOnce() + Send + 'static) {
        self.on_released = Some(Box::new(callback));
    }

    fn launch(&mut self, receiver: Receiver<LoopMessage>) -> io::Result<()> {
        let runtime = (self.factory)(Arc::clone(&self.shared), receiver);
        let (exit_tx, exit_rx) = mpsc::sync_channel(1);
        let status = Arc::clone(&self.status);
        let release = Arc::clone(&self.direct_release);
        *lock(&status) = ControlStatus::Running;
        thread::Builder::new()
            .name(CONTROL_THREAD.into())
            .spawn(move || {
                let exit = match panic::catch_unwind(AssertUnwindSafe(move || runtime.run())) {
                    Ok(released) => Exit::Finished { released },
                    Err(_) => {
                        let message = if release(DIRECT_RELEASE_BUDGET) {
                            "Fan control stopped after an internal error. Fans are under BIOS control."
                        } else {
                            "Fan control stopped after an internal error and could not confirm that fans returned to BIOS control."
                        };
                        *lock(&status) = ControlStatus::Stopped(message.to_owned());
                        Exit::Panicked
                    }
                };
                exit_tx.send(exit).ok();
            })?;
        self.exit = Some(exit_rx);
        Ok(())
    }

    pub fn restart(&mut self) -> io::Result<()> {
        if self.finished.is_some() || self.status() == ControlStatus::Running {
            return Ok(());
        }
        let (sender, receiver) = mpsc::channel();
        *lock(&self.sender) = sender;
        self.launch(receiver)
    }

    pub fn remote(&self) -> Remote {
        Remote {
            sender: Arc::clone(&self.sender),
            shared: Arc::clone(&self.shared),
        }
    }

    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    pub fn status(&self) -> ControlStatus {
        lock(&self.status).clone()
    }

    pub fn view(&self) -> (ControlStatus, Option<Arc<ControlState>>) {
        (self.status(), self.shared.state())
    }

    fn push(&self, kind: RequestKind, outcome: Result<(), String>) {
        lock(&self.results).push(CommandResult { kind, outcome });
    }

    pub fn send(&self, request: Request) {
        let kind = request.kind();
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        let message = match request {
            Request::ReplaceConfig(config) => {
                if let Err(error) = config.validate() {
                    self.push(kind, Err(error.to_string()));
                    return;
                }
                LoopMessage::ReplaceConfig(config, reply_tx)
            }
            Request::SetActiveProfile(name) => LoopMessage::SetActiveProfile(name, reply_tx),
            Request::Identify(fan) => {
                reply_tx.send(Ok(())).ok();
                LoopMessage::Identify(fan)
            }
        };
        if self.status() != ControlStatus::Running || lock(&self.sender).send(message).is_err() {
            self.push(kind, Err(NOT_RUNNING.to_owned()));
            return;
        }
        lock(&self.pending).push(Pending {
            kind,
            reply: reply_rx,
            sent: Instant::now(),
        });
    }

    pub fn take_results(&self) -> Vec<CommandResult> {
        let mut results = std::mem::take(&mut *lock(&self.results));
        lock(&self.pending).retain(|pending| {
            let outcome = match pending.reply.try_recv() {
                Ok(outcome) => outcome,
                Err(TryRecvError::Empty) if pending.sent.elapsed() < REPLY_TIMEOUT => return true,
                Err(TryRecvError::Empty) => Err("timed out waiting for fan control".to_owned()),
                Err(TryRecvError::Disconnected) => Err(NOT_RUNNING.to_owned()),
            };
            results.push(CommandResult {
                kind: pending.kind,
                outcome,
            });
            false
        });
        results
    }

    pub fn shutdown(&mut self) -> bool {
        if let Some(released) = self.finished {
            return released;
        }
        lock(&self.sender).send(LoopMessage::Stop).ok();
        let exit = self
            .exit
            .take()
            .and_then(|exit| exit.recv_timeout(QUIT_WAIT).ok());
        let released = match exit {
            Some(Exit::Finished { released: true }) => true,
            other => {
                tracing::warn!(
                    ?other,
                    "fan control did not confirm the release; releasing directly and leaving the rest to the guardian"
                );
                self.shared.halt();
                if !(self.direct_release)(DIRECT_RELEASE_BUDGET) {
                    tracing::error!("direct release failed");
                }
                false
            }
        };
        self.finished = Some(released);
        if released && let Some(callback) = self.on_released.take() {
            callback();
        }
        released
    }
}

impl<B: Backend + Send + 'static> Drop for Controller<B> {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use fanforge_core::store;
    use fanforge_hw::ite::fake::{FakeIte, SharedFake};
    use fanforge_hw::system::Hardware;

    use super::*;
    use crate::conflicts::ConflictMonitor;
    use crate::runtime::Paths;
    use crate::runtime::tests::{config_with, manual, opener};

    const CHA1_CONTROL: u8 = 0x16;
    const AUTO: u8 = 0x80;

    struct Rig {
        controller: Controller<Hardware<SharedFake>>,
        fake: SharedFake,
        direct_releases: Arc<AtomicUsize>,
        _dir: tempfile::TempDir,
    }

    fn rig(tag: &'static str, panics: usize) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config.toml"),
            snapshot: dir.path().join("snapshot.toml"),
        };
        store::save_toml_atomic(&paths.config, &config_with(&[("pwm2", manual(60.0))]), 0).unwrap();
        let fake = SharedFake::new(FakeIte::new());
        let factory_fake = fake.clone();
        let launches = AtomicUsize::new(0);
        let factory: RuntimeFactory<Hardware<SharedFake>> = Box::new(move |shared, messages| {
            let launch = launches.fetch_add(1, Ordering::SeqCst);
            let mut open = opener(factory_fake.clone(), tag);
            let opener = Box::new(move |snapshot: &Path| {
                assert!(launch >= panics, "simulated runtime failure");
                open(snapshot)
            });
            Runtime::new(
                paths.clone(),
                shared,
                messages,
                opener,
                ConflictMonitor::with_processes(Box::new(|| Ok(Vec::new()))),
            )
        });
        let direct_releases = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&direct_releases);
        let controller = Controller::start(
            Arc::new(Shared::default()),
            factory,
            Arc::new(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                true
            }),
        )
        .unwrap();
        Rig {
            controller,
            fake,
            direct_releases,
            _dir: dir,
        }
    }

    fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn collect(controller: &Controller<Hardware<SharedFake>>, count: usize) -> Vec<CommandResult> {
        let mut results = Vec::new();
        wait_until(|| {
            results.extend(controller.take_results());
            results.len() >= count
        });
        results
    }

    #[test]
    fn requests_are_tagged_and_answered() {
        let rig = rig("requests", 0);
        assert!(wait_until(|| rig.controller.view().1.is_some()));
        let config = rig.controller.view().1.unwrap().config.clone();
        rig.controller
            .send(Request::ReplaceConfig(Box::new(config)));
        rig.controller
            .send(Request::SetActiveProfile("Missing".to_owned()));
        rig.controller.send(Request::Identify("pwm1".into()));
        let mut results = collect(&rig.controller, 3);
        results.sort_by_key(|result| result.kind as u8);
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].kind, RequestKind::Config);
        assert_eq!(results[0].outcome, Ok(()));
        assert_eq!(results[1].kind, RequestKind::Profile);
        assert!(results[1].outcome.is_err());
        assert_eq!(results[2].kind, RequestKind::Identify);
        assert_eq!(results[2].outcome, Ok(()));
    }

    #[test]
    fn invalid_configs_never_reach_the_loop() {
        let rig = rig("invalid", 0);
        let invalid = Config {
            interval_ms: 1,
            ..Config::default()
        };
        rig.controller
            .send(Request::ReplaceConfig(Box::new(invalid)));
        let results = rig.controller.take_results();
        assert_eq!(results.len(), 1);
        assert!(
            results[0]
                .outcome
                .as_ref()
                .unwrap_err()
                .contains("interval")
        );
    }

    #[test]
    fn quitting_releases_fans_then_reports_released() {
        let mut rig = rig("quit", 0);
        let fake = rig.fake.clone();
        assert!(wait_until(|| fake.with(|chip| chip.reg(CHA1_CONTROL))
            & AUTO
            == 0));
        let reported = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&reported);
        rig.controller
            .on_released(move || flag.store(true, Ordering::SeqCst));
        assert!(rig.controller.shutdown());
        assert!(reported.load(Ordering::SeqCst));
        assert_eq!(rig.fake.with(|chip| chip.reg(CHA1_CONTROL)) & AUTO, AUTO);
        assert_eq!(rig.direct_releases.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn an_unconfirmed_quit_never_tells_the_guardian_it_was_released() {
        let mut rig = rig("unconfirmed", usize::MAX);
        assert!(wait_until(|| matches!(
            rig.controller.status(),
            ControlStatus::Stopped(_)
        )));
        let reported = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&reported);
        rig.controller
            .on_released(move || flag.store(true, Ordering::SeqCst));
        assert!(!rig.controller.shutdown());
        assert!(!reported.load(Ordering::SeqCst));
        assert!(rig.controller.shared().is_halted());
        assert_eq!(rig.direct_releases.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_crashed_runtime_releases_directly_and_can_restart() {
        let mut rig = rig("crash", 1);
        assert!(wait_until(|| matches!(
            rig.controller.status(),
            ControlStatus::Stopped(_)
        )));
        assert_eq!(rig.direct_releases.load(Ordering::SeqCst), 1);
        rig.controller.send(Request::Identify("pwm1".into()));
        assert_eq!(
            rig.controller.take_results()[0].outcome,
            Err(NOT_RUNNING.to_owned())
        );
        rig.controller.restart().unwrap();
        assert_eq!(rig.controller.status(), ControlStatus::Running);
        let fake = rig.fake.clone();
        assert!(wait_until(|| fake.with(|chip| chip.reg(CHA1_CONTROL))
            & AUTO
            == 0));
        assert!(rig.controller.shutdown());
    }
}
