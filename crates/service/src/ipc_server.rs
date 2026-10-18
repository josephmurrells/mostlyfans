use std::io;
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

use fanforge_core::paths::PIPE_NAME;
use fanforge_core::protocol::{FrameError, Request, Response, read_frame, write_frame};
use fanforge_ipc::{PipeListener, PipeStream};

use crate::shared::{LoopMessage, Shared};

const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

pub fn spawn(shared: Arc<Shared>, loop_tx: Sender<LoopMessage>) -> io::Result<()> {
    spawn_on(PIPE_NAME, shared, loop_tx)
}

pub fn spawn_on(
    pipe_name: &str,
    shared: Arc<Shared>,
    loop_tx: Sender<LoopMessage>,
) -> io::Result<()> {
    let mut listener = PipeListener::bind(pipe_name)?;
    thread::Builder::new()
        .name("ipc-accept".into())
        .spawn(move || {
            loop {
                match listener.accept() {
                    Ok(stream) => {
                        let shared = Arc::clone(&shared);
                        let loop_tx = loop_tx.clone();
                        let spawned = thread::Builder::new()
                            .name("ipc-client".into())
                            .spawn(move || serve(stream, &shared, &loop_tx));
                        if let Err(error) = spawned {
                            tracing::error!(%error, "failed to spawn IPC client thread");
                        }
                    }
                    Err(error) => {
                        tracing::error!(%error, "failed to accept IPC client");
                        thread::sleep(Duration::from_secs(1));
                    }
                }
            }
        })?;
    Ok(())
}

fn serve(mut stream: PipeStream, shared: &Shared, loop_tx: &Sender<LoopMessage>) {
    loop {
        let request = match read_frame::<_, Request>(&mut stream) {
            Ok(request) => request,
            Err(FrameError::Closed) => return,
            Err(error) => {
                tracing::warn!(%error, "dropping IPC client after a bad frame");
                return;
            }
        };
        let response = handle(request, shared, loop_tx);
        if let Err(error) = write_frame(&mut stream, &response) {
            tracing::debug!(%error, "IPC client went away");
            return;
        }
    }
}

fn handle(request: Request, shared: &Shared, loop_tx: &Sender<LoopMessage>) -> Response {
    match request {
        Request::GetState => match shared.state.read().ok().and_then(|state| state.clone()) {
            Some(state) => Response::State(Box::new(state)),
            None => error("the service is still starting"),
        },
        Request::ReplaceConfig(config) => {
            if let Err(validation) = config.validate() {
                return error(validation.to_string());
            }
            round_trip(loop_tx, |reply| LoopMessage::ReplaceConfig(config, reply))
        }
        Request::SetActiveProfile { name } => {
            round_trip(loop_tx, |reply| LoopMessage::SetActiveProfile(name, reply))
        }
        Request::Identify { fan } => match loop_tx.send(LoopMessage::Identify(fan)) {
            Ok(()) => Response::Ok,
            Err(_) => error("the control loop has stopped"),
        },
    }
}

fn round_trip(
    loop_tx: &Sender<LoopMessage>,
    message: impl FnOnce(mpsc::SyncSender<Result<(), String>>) -> LoopMessage,
) -> Response {
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    if loop_tx.send(message(reply_tx)).is_err() {
        return error("the control loop has stopped");
    }
    match reply_rx.recv_timeout(REPLY_TIMEOUT) {
        Ok(Ok(())) => Response::Ok,
        Ok(Err(message)) => error(message),
        Err(_) => error("timed out waiting for the control loop"),
    }
}

fn error(message: impl Into<String>) -> Response {
    Response::Error {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use fanforge_core::config::Config;
    use fanforge_ipc::{Client, ServerCheck};

    use super::*;

    #[test]
    fn validates_configs_before_reaching_the_loop() {
        let shared = Arc::new(Shared::new());
        let (loop_tx, loop_rx) = mpsc::channel();
        let name = format!(r"\\.\pipe\fanforge-service-test-{}", std::process::id());
        spawn_on(&name, Arc::clone(&shared), loop_tx).unwrap();

        let mut client = Client::connect(&name, &ServerCheck::AnyProcess).unwrap();
        assert!(matches!(
            client.request(&Request::GetState).unwrap(),
            Response::Error { .. }
        ));

        let invalid = Config {
            interval_ms: 1,
            ..Config::default()
        };
        let response = client
            .request(&Request::ReplaceConfig(Box::new(invalid)))
            .unwrap();
        assert!(matches!(response, Response::Error { message } if message.contains("interval")));
        assert!(loop_rx.try_recv().is_err());

        let responder = thread::spawn(move || match loop_rx.recv().unwrap() {
            LoopMessage::ReplaceConfig(_, reply) => reply.send(Ok(())).unwrap(),
            _ => panic!("unexpected message"),
        });
        let response = client
            .request(&Request::ReplaceConfig(Box::default()))
            .unwrap();
        assert_eq!(response, Response::Ok);
        responder.join().unwrap();
    }
}
