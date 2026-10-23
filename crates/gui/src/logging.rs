use std::sync::{Mutex, PoisonError};

use mostlyfans_win::paths;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;

const MAX_LOG_FILES: usize = 7;

static GUARD: Mutex<Option<WorkerGuard>> = Mutex::new(None);

fn filter() -> EnvFilter {
    EnvFilter::try_from_env("MOSTLYFANS_LOG").unwrap_or_else(|_| EnvFilter::new("info"))
}

pub fn init(prefix: &str) {
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(prefix)
        .filename_suffix("log")
        .max_log_files(MAX_LOG_FILES)
        .build(paths::log_dir());
    match appender {
        Ok(appender) => {
            let (writer, guard) = tracing_appender::non_blocking(appender);
            tracing_subscriber::fmt()
                .with_env_filter(filter())
                .with_ansi(false)
                .with_writer(writer)
                .init();
            *GUARD.lock().unwrap_or_else(PoisonError::into_inner) = Some(guard);
        }
        Err(_) => tracing_subscriber::fmt().with_env_filter(filter()).init(),
    }
}

pub fn flush() {
    drop(GUARD.lock().unwrap_or_else(PoisonError::into_inner).take());
}
