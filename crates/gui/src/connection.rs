use std::io;
use std::sync::Arc;

use fanforge_control::backend::Backend;
use fanforge_control::controller::{CommandResult, Controller};
use fanforge_core::state::ControlState;

pub use fanforge_control::controller::{ControlStatus, Request, RequestKind};

pub trait ControlLink {
    fn view(&self) -> (ControlStatus, Option<Arc<ControlState>>);
    fn send(&self, request: Request);
    fn take_results(&self) -> Vec<CommandResult>;
    fn restart(&mut self) -> io::Result<()>;
}

impl<B: Backend + Send + 'static> ControlLink for Controller<B> {
    fn view(&self) -> (ControlStatus, Option<Arc<ControlState>>) {
        Controller::view(self)
    }

    fn send(&self, request: Request) {
        Controller::send(self, request);
    }

    fn take_results(&self) -> Vec<CommandResult> {
        Controller::take_results(self)
    }

    fn restart(&mut self) -> io::Result<()> {
        Controller::restart(self)
    }
}
