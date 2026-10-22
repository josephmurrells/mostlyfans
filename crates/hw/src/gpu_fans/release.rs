use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use super::ownership::{self, Loaded};
use super::{DriverOpener, FanDriver, NOT_RESPONDING};

struct Spare {
    opener: DriverOpener,
    driver: Option<Box<dyn FanDriver>>,
    path: PathBuf,
}

#[derive(Clone)]
pub struct GpuReleaser {
    spare: Arc<Mutex<Spare>>,
}

impl GpuReleaser {
    pub fn open(opener: DriverOpener, path: PathBuf) -> Self {
        let driver = opener()
            .map_err(|error| tracing::info!(%error, "no spare GPU driver session"))
            .ok();
        Self {
            spare: Arc::new(Mutex::new(Spare {
                opener,
                driver,
                path,
            })),
        }
    }

    pub fn has_session(&self) -> bool {
        self.spare
            .try_lock()
            .is_ok_and(|spare| spare.driver.is_some())
    }

    pub fn release(&self, budget: Duration) -> Result<(), String> {
        let (sender, receiver) = mpsc::channel();
        let spare = Arc::clone(&self.spare);
        thread::Builder::new()
            .name("gpu-release".into())
            .spawn(move || {
                let mut spare = spare.lock().unwrap_or_else(PoisonError::into_inner);
                let result = panic::catch_unwind(AssertUnwindSafe(|| release(&mut spare)))
                    .unwrap_or_else(|_| {
                        spare.driver = None;
                        Err("the GPU driver panicked".to_owned())
                    });
                sender.send(result).ok();
            })
            .map_err(|error| error.to_string())?;
        receiver
            .recv_timeout(budget)
            .unwrap_or_else(|_| Err(NOT_RESPONDING.to_owned()))
    }
}

fn release(spare: &mut Spare) -> Result<(), String> {
    let loaded = ownership::load(&spare.path);
    if let Loaded::Owned(ownership) = &loaded
        && ownership.fans.is_empty()
    {
        return Ok(());
    }
    if spare.driver.is_none() {
        spare.driver = Some((spare.opener)()?);
    }
    let Some(driver) = &spare.driver else {
        return Ok(());
    };
    let mut first_error = None;
    for gpu in driver.gpus() {
        for index in 0..gpu.fans {
            let ours = match &loaded {
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
    if first_error.is_some() {
        spare.driver = None;
    }
    first_error.map_or(Ok(()), Err)
}
