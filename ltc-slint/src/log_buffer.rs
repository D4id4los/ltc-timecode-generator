use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chrono::Local;
use log::{LevelFilter, Log, Metadata, Record, SetLoggerError};

const MAX_LOG_ENTRIES: usize = 1000;

pub struct LogBuffer {
    pub entries: VecDeque<String>,
    max_entries: usize,
}

impl LogBuffer {
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(max_entries + 1),
            max_entries,
        }
    }

    fn push(&mut self, entry: String) {
        if self.entries.len() >= self.max_entries {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }
}

struct CapturingLogger {
    buffer: Arc<Mutex<LogBuffer>>,
    stderr_logger: env_logger::Logger,
}

impl Log for CapturingLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.stderr_logger.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            let formatted = format!(
                "[{}] [{}] [{}] {}",
                Local::now().format("%H:%M:%S%.3f"),
                record.level(),
                record.target(),
                record.args()
            );

            if let Ok(mut buffer) = self.buffer.lock() {
                buffer.push(formatted);
            }

            self.stderr_logger.log(record);
        }
    }

    fn flush(&self) {
        self.stderr_logger.flush();
    }
}

pub fn init_logger(
    filter: &str,
) -> Result<Arc<Mutex<LogBuffer>>, SetLoggerError> {
    let buffer = Arc::new(Mutex::new(LogBuffer::new(MAX_LOG_ENTRIES)));

    let stderr_logger = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(filter),
    )
    .format_timestamp_millis()
    .build();

    let logger = CapturingLogger {
        buffer: buffer.clone(),
        stderr_logger,
    };

    log::set_boxed_logger(Box::new(logger))?;
    log::set_max_level(LevelFilter::Trace);

    Ok(buffer)
}