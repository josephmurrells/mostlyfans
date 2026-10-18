use std::env;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use fanforge_core::FanId;
use fanforge_core::config::Config;
use fanforge_core::paths::{PIPE_NAME, SERVICE_NAME};
use fanforge_core::protocol::{Request, Response, ServiceState};
use fanforge_ipc::{Client, ClientError, ServerCheck};

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
const DEV_ENV: &str = "FANFORGE_DEV_ANY_SERVER";

#[derive(Clone, Debug, PartialEq)]
pub enum LinkStatus {
    Connecting,
    Connected,
    Disconnected(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestKind {
    Config,
    Profile,
    Identify,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CommandResult {
    pub kind: RequestKind,
    pub outcome: Result<(), String>,
}

#[derive(Debug)]
pub enum Outgoing {
    ReplaceConfig(Box<Config>),
    SetActiveProfile(String),
    Identify(FanId),
}

impl Outgoing {
    fn kind(&self) -> RequestKind {
        match self {
            Self::ReplaceConfig(_) => RequestKind::Config,
            Self::SetActiveProfile(_) => RequestKind::Profile,
            Self::Identify(_) => RequestKind::Identify,
        }
    }

    fn into_request(self) -> Request {
        match self {
            Self::ReplaceConfig(config) => Request::ReplaceConfig(config),
            Self::SetActiveProfile(name) => Request::SetActiveProfile { name },
            Self::Identify(fan) => Request::Identify { fan },
        }
    }
}

#[derive(Debug)]
pub struct Snapshot {
    pub link: LinkStatus,
    pub state: Option<Arc<ServiceState>>,
    results: Vec<CommandResult>,
}

#[derive(Clone, Debug)]
pub struct Connection {
    shared: Arc<Mutex<Snapshot>>,
    outgoing: Sender<Outgoing>,
}

impl Connection {
    pub fn start(repaint: impl Fn() + Send + 'static) -> Self {
        let shared = Arc::new(Mutex::new(Snapshot {
            link: LinkStatus::Connecting,
            state: None,
            results: Vec::new(),
        }));
        let (outgoing, incoming) = mpsc::channel();
        let worker_shared = Arc::clone(&shared);
        let spawned = thread::Builder::new()
            .name("service-link".into())
            .spawn(move || worker(&worker_shared, &incoming, &repaint));
        if let Err(error) = spawned {
            lock(&shared).link =
                LinkStatus::Disconnected(format!("failed to start connection thread: {error}"));
        }
        Self { shared, outgoing }
    }

    pub fn view(&self) -> (LinkStatus, Option<Arc<ServiceState>>) {
        let snapshot = lock(&self.shared);
        (snapshot.link.clone(), snapshot.state.clone())
    }

    pub fn send(&self, message: Outgoing) {
        let kind = message.kind();
        if self.outgoing.send(message).is_err() {
            lock(&self.shared).results.push(CommandResult {
                kind,
                outcome: Err("connection thread stopped".to_owned()),
            });
        }
    }

    pub fn take_results(&self) -> Vec<CommandResult> {
        std::mem::take(&mut lock(&self.shared).results)
    }
}

fn lock(shared: &Mutex<Snapshot>) -> MutexGuard<'_, Snapshot> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

fn server_check() -> ServerCheck {
    if cfg!(debug_assertions) && env::var_os(DEV_ENV).is_some() {
        ServerCheck::AnyProcess
    } else {
        ServerCheck::Service(SERVICE_NAME.to_owned())
    }
}

fn worker(shared: &Mutex<Snapshot>, incoming: &Receiver<Outgoing>, repaint: &dyn Fn()) {
    let check = server_check();
    loop {
        match Client::connect(PIPE_NAME, &check) {
            Ok(mut client) => {
                lock(shared).link = LinkStatus::Connected;
                repaint();
                let error = session(&mut client, shared, incoming, repaint);
                let mut snapshot = lock(shared);
                snapshot.link = LinkStatus::Disconnected(error.to_string());
                snapshot.state = None;
            }
            Err(error) => lock(shared).link = LinkStatus::Disconnected(error.to_string()),
        }
        repaint();
        let deadline = Instant::now() + RECONNECT_DELAY;
        while let Some(wait) = deadline.checked_duration_since(Instant::now()) {
            match incoming.recv_timeout(wait) {
                Ok(message) => {
                    lock(shared).results.push(CommandResult {
                        kind: message.kind(),
                        outcome: Err("not connected to the FanForge service".to_owned()),
                    });
                    repaint();
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

fn session(
    client: &mut Client,
    shared: &Mutex<Snapshot>,
    incoming: &Receiver<Outgoing>,
    repaint: &dyn Fn(),
) -> ClientError {
    loop {
        match client.request(&Request::GetState) {
            Ok(Response::State(state)) => {
                let mut snapshot = lock(shared);
                snapshot.link = LinkStatus::Connected;
                snapshot.state = Some(Arc::from(state));
            }
            Ok(Response::Error { .. }) => lock(shared).link = LinkStatus::Connecting,
            Ok(Response::Ok) => {}
            Err(error) => return error,
        }
        repaint();
        let message = match incoming.recv_timeout(POLL_INTERVAL) {
            Ok(message) => message,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return ClientError::NotRunning,
        };
        let kind = message.kind();
        let outcome = match client.request(&message.into_request()) {
            Ok(Response::Error { message }) => Err(message),
            Ok(_) => Ok(()),
            Err(error) => return error,
        };
        lock(shared).results.push(CommandResult { kind, outcome });
        repaint();
    }
}
