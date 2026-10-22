use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use super::{DriverOpener, FanDriver, FanSample, GpuDesc};

pub const GPU_ID: &str = "gpu/nvidia/gpu-test";
pub const GPU_KEY: &str = "GPU-TEST";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FakeFan {
    pub manual: bool,
    pub target: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Write {
    Speed(u32, u32),
    Restore(u32),
}

#[derive(Debug)]
pub struct FakeGpus {
    pub gpus: Vec<GpuDesc>,
    pub fans: BTreeMap<u32, FakeFan>,
    pub writes: Vec<Write>,
    pub opens: u32,
    pub fail_open: bool,
    pub fail_writes: bool,
    pub fail_samples: bool,
    pub panic_on_sample: bool,
    pub no_target: bool,
    pub expired_before: u32,
    pub held: bool,
}

impl Default for FakeGpus {
    fn default() -> Self {
        Self {
            gpus: vec![GpuDesc {
                id: GPU_ID.to_owned(),
                key: GPU_KEY.to_owned(),
                label: "Test RTX".to_owned(),
                fans: 2,
                range: Some((30, 100)),
            }],
            fans: [(0, FakeFan::default()), (1, FakeFan::default())].into(),
            writes: Vec::new(),
            opens: 0,
            fail_open: false,
            fail_writes: false,
            fail_samples: false,
            panic_on_sample: false,
            no_target: false,
            expired_before: 0,
            held: false,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SharedGpus {
    inner: Arc<(Mutex<FakeGpus>, Condvar)>,
}

impl SharedGpus {
    pub fn new(gpus: FakeGpus) -> Self {
        Self {
            inner: Arc::new((Mutex::new(gpus), Condvar::new())),
        }
    }

    fn lock(&self) -> MutexGuard<'_, FakeGpus> {
        self.inner.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn with<T>(&self, change: impl FnOnce(&mut FakeGpus) -> T) -> T {
        let result = change(&mut self.lock());
        self.inner.1.notify_all();
        result
    }

    pub fn fan(&self, index: u32) -> FakeFan {
        self.lock().fans[&index]
    }

    pub fn hold(&self) {
        self.with(|gpus| gpus.held = true);
    }

    pub fn unhold(&self) {
        self.with(|gpus| gpus.held = false);
    }

    fn enter(&self) -> MutexGuard<'_, FakeGpus> {
        let guard = self.lock();
        self.inner
            .1
            .wait_while(guard, |gpus| gpus.held)
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub fn opener(&self) -> DriverOpener {
        let shared = self.clone();
        Arc::new(move || {
            let mut gpus = shared.enter();
            gpus.opens += 1;
            if gpus.fail_open {
                return Err("no NVIDIA driver in tests".to_owned());
            }
            Ok(Box::new(FakeDriver {
                shared: shared.clone(),
                generation: gpus.opens,
            }) as Box<dyn FanDriver>)
        })
    }
}

struct FakeDriver {
    shared: SharedGpus,
    generation: u32,
}

impl FakeDriver {
    fn enter(&self) -> Result<MutexGuard<'_, FakeGpus>, String> {
        let gpus = self.shared.enter();
        if self.generation < gpus.expired_before {
            return Err("the driver was reloaded".to_owned());
        }
        Ok(gpus)
    }
}

impl FanDriver for FakeDriver {
    fn gpus(&self) -> Vec<GpuDesc> {
        self.shared.enter().gpus.clone()
    }

    fn sample(&self, _gpu: &GpuDesc, fan: u32) -> Result<FanSample, String> {
        let gpus = self.enter()?;
        if gpus.panic_on_sample {
            drop(gpus);
            panic!("fake NVML panicked");
        }
        if gpus.fail_samples {
            return Err("GPU is lost".to_owned());
        }
        let state = gpus.fans.get(&fan).copied().ok_or("no such fan")?;
        Ok(FanSample {
            percent: Some(state.target),
            target: (!gpus.no_target).then_some(state.target),
            rpm: Some(state.target * 30),
            manual: Some(state.manual),
        })
    }

    fn set_speed(&self, _gpu: &GpuDesc, fan: u32, percent: u32) -> Result<(), String> {
        let mut gpus = self.enter()?;
        if gpus.fail_writes {
            return Err("insufficient permissions".to_owned());
        }
        gpus.writes.push(Write::Speed(fan, percent));
        let state = gpus.fans.get_mut(&fan).ok_or("no such fan")?;
        state.manual = true;
        state.target = percent;
        Ok(())
    }

    fn restore(&self, _gpu: &GpuDesc, fan: u32) -> Result<(), String> {
        let mut gpus = self.enter()?;
        if gpus.fail_writes {
            return Err("insufficient permissions".to_owned());
        }
        gpus.writes.push(Write::Restore(fan));
        let state = gpus.fans.get_mut(&fan).ok_or("no such fan")?;
        state.manual = false;
        state.target = 30;
        Ok(())
    }
}
