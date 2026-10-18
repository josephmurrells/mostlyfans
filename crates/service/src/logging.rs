use fanforge_core::paths;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;

const MAX_LOG_FILES: usize = 7;

fn filter() -> EnvFilter {
    EnvFilter::try_from_env("FANFORGE_LOG").unwrap_or_else(|_| EnvFilter::new("info"))
}

pub fn init_console() {
    tracing_subscriber::fmt().with_env_filter(filter()).init();
}

pub fn init_file() -> Option<WorkerGuard> {
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("fanforge")
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
            Some(guard)
        }
        Err(_) => {
            init_console();
            None
        }
    }
}
