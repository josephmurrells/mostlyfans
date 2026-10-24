use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use mostlyfans_control::guardian::{NO_HARDWARE_EXIT_CODE, Signal, SignalSink};
use mostlyfans_win::{
    AnonymousPipe, Breakaway, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    PROCESS_TERMINATE, Spawned,
};

use crate::notices::{Notices, Topic};

const RESPAWN_LIMIT: usize = 3;
const RESPAWN_WINDOW: Duration = Duration::from_secs(10 * 60);
const RESPAWN_PAUSE: Duration = Duration::from_secs(1);

fn guardian_arguments(values: &[usize]) -> Vec<String> {
    let mut arguments = vec!["--guardian".to_owned()];
    for (flag, value) in ["--parent-handle", "--pipe-handle"].into_iter().zip(values) {
        arguments.push(flag.to_owned());
        arguments.push(value.to_string());
    }
    arguments
}

fn spawn_guardian(executable: &Path) -> io::Result<(Spawned, File)> {
    let pipe = AnonymousPipe::new()?;
    let writer = mostlyfans_win::nonblocking_writer(pipe.write)?;
    let parent = mostlyfans_win::duplicate_current_process(
        PROCESS_SYNCHRONIZE | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
    )?;
    let spawned =
        mostlyfans_win::spawn_with_handles(executable, &[&parent, &pipe.read], guardian_arguments)?;
    Ok((spawned, writer))
}

struct Inner {
    executable: PathBuf,
    _executable_lock: Option<File>,
    autostarted: bool,
    supervising: AtomicBool,
    writer: Mutex<Option<File>>,
    stopping: AtomicBool,
    notices: Notices,
}

impl Inner {
    fn writer(&self) -> MutexGuard<'_, Option<File>> {
        self.writer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn send(&self, signal: Signal) {
        if let Some(writer) = self.writer().as_mut()
            && let Err(error) = writer.write(&[signal.encode()])
        {
            tracing::debug!(%error, "guardian pipe write failed");
        }
    }

    fn supervise(&self) {
        let mut restarts: VecDeque<Instant> = VecDeque::new();
        loop {
            if self.stopping.load(Ordering::SeqCst) {
                return;
            }
            restarts.retain(|at| at.elapsed() < RESPAWN_WINDOW);
            if restarts.len() > RESPAWN_LIMIT {
                self.notices.set(
                    Topic::Guardian,
                    "The guardian keeps stopping, so MostlyFans is unprotected against forced termination: if it is killed, fans stay at their last speed until the next start.",
                );
                return;
            }
            restarts.push_back(Instant::now());
            let (spawned, writer) = match spawn_guardian(&self.executable) {
                Ok(started) => started,
                Err(error) => {
                    tracing::error!(%error, "could not start the guardian");
                    self.notices.set(
                        Topic::Guardian,
                        format!("The guardian could not start ({error}); MostlyFans is unprotected against forced termination."),
                    );
                    thread::sleep(RESPAWN_PAUSE);
                    continue;
                }
            };
            *self.writer() = Some(writer);
            self.notices.clear(Topic::Guardian);
            if spawned.breakaway == Breakaway::Denied {
                let in_job = mostlyfans_win::current_process_in_job();
                tracing::warn!(?in_job, "guardian could not leave the app's job");
                if !self.autostarted {
                    self.notices.set(
                        Topic::GuardianJob,
                        "The guardian shares the app's job, so closing that job ends both and fans keep their last speed.",
                    );
                }
            } else {
                self.notices.clear(Topic::GuardianJob);
            }
            tracing::info!(pid = spawned.process.id(), "guardian started");
            spawned.process.wait(None).ok();
            self.writer().take();
            if self.stopping.load(Ordering::SeqCst) {
                return;
            }
            let code = spawned.process.exit_code().ok().flatten();
            if code == Some(NO_HARDWARE_EXIT_CODE) {
                self.notices.set(
                    Topic::Guardian,
                    "The guardian could not open the fan controller, so MostlyFans is unprotected against forced termination.",
                );
                return;
            }
            tracing::warn!(?code, "guardian exited; starting a new one");
            thread::sleep(RESPAWN_PAUSE);
        }
    }
}

#[derive(Clone)]
pub struct GuardianLink {
    inner: Arc<Inner>,
}

impl GuardianLink {
    pub fn start(executable: PathBuf, autostarted: bool, notices: Notices) -> Self {
        let executable_lock = mostlyfans_win::lock_against_replacement(&executable)
            .map_err(
                |error| tracing::warn!(%error, "could not lock the executable against replacement"),
            )
            .ok();
        let inner = Arc::new(Inner {
            executable,
            _executable_lock: executable_lock,
            autostarted,
            supervising: AtomicBool::new(false),
            writer: Mutex::new(None),
            stopping: AtomicBool::new(false),
            notices,
        });
        let link = Self { inner };
        link.ensure_running();
        link
    }

    pub fn ensure_running(&self) {
        if self.inner.stopping.load(Ordering::SeqCst)
            || self.inner.supervising.swap(true, Ordering::SeqCst)
        {
            return;
        }
        let supervisor = Arc::clone(&self.inner);
        let spawned = thread::Builder::new()
            .name("guardian-supervisor".into())
            .spawn(move || {
                supervisor.supervise();
                supervisor.supervising.store(false, Ordering::SeqCst);
            });
        if let Err(error) = spawned {
            self.inner.supervising.store(false, Ordering::SeqCst);
            self.inner.notices.set(
                Topic::Guardian,
                format!("The guardian could not start ({error}); MostlyFans is unprotected against forced termination."),
            );
        }
    }

    pub fn sink(&self) -> SignalSink {
        let inner = Arc::clone(&self.inner);
        Arc::new(move |signal| inner.send(signal))
    }

    pub fn released(&self) {
        self.inner.stopping.store(true, Ordering::SeqCst);
        self.inner.send(Signal::Released);
        self.inner.writer().take();
    }
}
