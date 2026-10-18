use fanforge_control::controller::Controller;
use fanforge_hw::system::Hardware;

pub use fanforge_control::controller::{ControlStatus, Request, RequestKind};

pub type Connection = Controller<Hardware>;
