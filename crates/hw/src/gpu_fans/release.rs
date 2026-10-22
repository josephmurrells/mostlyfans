use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, TryLockError};
use std::thread;
use std::time::Duration;

use super::ownership::{self, Loaded};
use super::{DriverOpener, FanDriver, NOT_RESPONDING, writes};

type Session = Option<Box<dyn FanDriver>>;

#[derive(Clone)]
pub struct GpuReleaser {
    opener: DriverOpener,
    path: PathBuf,
    spare: Arc<Mutex<Session>>,
}

impl GpuReleaser {
    pub fn open(opener: DriverOpener, path: PathBuf) -> Self {
        let driver = opener()
            .map_err(|error| tracing::info!(%error, "no spare GPU driver session"))
            .ok();
        Self {
            opener,
            path,
            spare: Arc::new(Mutex::new(driver)),
        }
    }

    pub fn has_session(&self) -> bool {
        self.spare.try_lock().is_ok_and(|session| session.is_some())
    }

    pub fn release(&self, budget: Duration) -> Result<(), String> {
        let (sender, receiver) = mpsc::channel();
        let releaser = self.clone();
        thread::Builder::new()
            .name("gpu-release".into())
            .spawn(move || {
                let _writes = writes();
                let result = match releaser.spare.try_lock() {
                    Ok(mut session) => releaser.release_with(&mut session),
                    Err(TryLockError::Poisoned(poisoned)) => {
                        releaser.release_with(&mut poisoned.into_inner())
                    }
                    Err(TryLockError::WouldBlock) => releaser.release_with(&mut None),
                };
                sender.send(result).ok();
            })
            .map_err(|error| error.to_string())?;
        receiver
            .recv_timeout(budget)
            .unwrap_or_else(|_| Err(NOT_RESPONDING.to_owned()))
    }

    fn release_with(&self, session: &mut Session) -> Result<(), String> {
        let loaded = ownership::load(&self.path);
        if let Loaded::Owned(ownership) = &loaded
            && ownership.fans.is_empty()
        {
            return Ok(());
        }
        let fresh = session.is_none();
        let first = self.attempt(session, &loaded);
        if first.is_ok() || fresh {
            return first;
        }
        tracing::warn!("the spare GPU driver session failed; retrying with a fresh one");
        *session = None;
        self.attempt(session, &loaded)
    }

    fn attempt(&self, session: &mut Session, loaded: &Loaded) -> Result<(), String> {
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            if session.is_none() {
                *session = Some((self.opener)()?);
            }
            match session {
                Some(driver) => restore(driver.as_ref(), loaded),
                None => Ok(()),
            }
        }))
        .unwrap_or_else(|_| Err("the GPU driver panicked".to_owned()));
        if result.is_err() {
            *session = None;
        }
        result
    }
}

fn restore(driver: &dyn FanDriver, loaded: &Loaded) -> Result<(), String> {
    let mut first_error = None;
    for gpu in driver.gpus() {
        for index in 0..gpu.fans {
            let ours = match loaded {
                Loaded::Owned(ownership) => ownership.fans.contains(&gpu.fan_id(index)),
                Loaded::Lost => driver
                    .sample(&gpu, index)
                    .map_or(true, |sample| sample.manual == Some(true)),
            };
            if ours && let Err(error) = driver.restore(&gpu, index) {
                tracing::error!(fan = %gpu.fan_id(index), %error, "could not return a GPU fan to the driver");
                first_error.get_or_insert(error);
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}
