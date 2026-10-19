use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use eframe::egui;
use fanforge_control::controller::{DIRECT_RELEASE_BUDGET, QUIT_WAIT};
use fanforge_win::paths::INSTANCE_MUTEX;
use fanforge_win::{ADMIN_ONLY_SDDL, Event, InstanceMutex};

use crate::tray::reveal;

const EXIT_MARGIN: Duration = Duration::from_secs(4);
const SHOW_POLL: Duration = Duration::from_millis(250);

pub struct Instance {
    _mutex: InstanceMutex,
    show: ShowGate,
}

pub enum Claim {
    Primary(Instance),
    Revealed,
    Refused(String),
}

fn exit_wait() -> Duration {
    QUIT_WAIT + DIRECT_RELEASE_BUDGET + EXIT_MARGIN
}

fn show_event_name() -> io::Result<String> {
    Ok(format!(
        r"Local\FanForge.Show.{}",
        fanforge_win::current_session_id()?
    ))
}

fn refused(error: &io::Error) -> Claim {
    tracing::error!(%error, "single-instance check failed");
    Claim::Refused(format!(
        "FanForge could not check for another running copy: {error}"
    ))
}

fn primary(mutex: InstanceMutex) -> Claim {
    match show_event_name().and_then(|name| Event::create(&name, ADMIN_ONLY_SDDL)) {
        Ok(show) => Claim::Primary(Instance {
            _mutex: mutex,
            show: ShowGate(Arc::new(Mutex::new(Some(show)))),
        }),
        Err(error) => refused(&error),
    }
}

pub fn claim() -> Claim {
    let mutex = match InstanceMutex::create(INSTANCE_MUTEX, ADMIN_ONLY_SDDL) {
        Ok(mutex) => mutex,
        Err(error) => return refused(&error),
    };
    match mutex.acquire(Duration::ZERO) {
        Ok(true) => return primary(mutex),
        Ok(false) => {}
        Err(error) => return refused(&error),
    }
    let existing = show_event_name().and_then(|name| Event::open(&name));
    if let Ok(Some(event)) = existing
        && event.set().is_ok()
    {
        return Claim::Revealed;
    }
    match mutex.acquire(exit_wait()) {
        Ok(true) => primary(mutex),
        Ok(false) => Claim::Refused(
            "FanForge is already controlling fans in another user session.".to_owned(),
        ),
        Err(error) => refused(&error),
    }
}

impl Instance {
    pub fn show_gate(&self) -> ShowGate {
        self.show.clone()
    }
}

#[derive(Clone)]
pub struct ShowGate(Arc<Mutex<Option<Event>>>);

impl ShowGate {
    fn event(&self) -> MutexGuard<'_, Option<Event>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn close(&self) {
        self.event().take();
    }

    pub fn forward_to(&self, ctx: &egui::Context) -> io::Result<()> {
        let gate = self.clone();
        let ctx = ctx.clone();
        thread::Builder::new()
            .name("show-requests".into())
            .spawn(move || {
                loop {
                    let signalled = match gate.event().as_ref() {
                        Some(event) => event.wait(Some(SHOW_POLL)),
                        None => return,
                    };
                    match signalled {
                        Ok(true) => reveal(&ctx),
                        Ok(false) => {}
                        Err(_) => return,
                    }
                }
            })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_launch_outwaits_the_slowest_quit() {
        assert!(exit_wait() > QUIT_WAIT + DIRECT_RELEASE_BUDGET);
    }
}
