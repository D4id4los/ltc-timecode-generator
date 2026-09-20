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
    .build();

    let logger = CapturingLogger {
        buffer: buffer.clone(),
        stderr_logger,
    };

    log::set_boxed_logger(Box::new(logger))?;
    log::set_max_level(LevelFilter::Trace);

    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_buffer_new_creates_empty() {
        let buf = LogBuffer::new(10);
        assert!(buf.entries.is_empty());
        assert_eq!(buf.max_entries, 10);
    }

    #[test]
    fn test_log_buffer_push_adds_entry() {
        let mut buf = LogBuffer::new(10);
        buf.push("entry 1".to_string());
        assert_eq!(buf.entries.len(), 1);
        assert_eq!(buf.entries[0], "entry 1");
    }

    #[test]
    fn test_log_buffer_push_preserves_order() {
        let mut buf = LogBuffer::new(10);
        buf.push("first".to_string());
        buf.push("second".to_string());
        buf.push("third".to_string());
        assert_eq!(buf.entries.len(), 3);
        assert_eq!(buf.entries[0], "first");
        assert_eq!(buf.entries[1], "second");
        assert_eq!(buf.entries[2], "third");
    }

    #[test]
    fn test_log_buffer_overflow_removes_oldest() {
        let mut buf = LogBuffer::new(3);
        buf.push("a".to_string());
        buf.push("b".to_string());
        buf.push("c".to_string());
        buf.push("d".to_string());
        assert_eq!(buf.entries.len(), 3);
        assert_eq!(buf.entries[0], "b");
        assert_eq!(buf.entries[1], "c");
        assert_eq!(buf.entries[2], "d");
    }

    #[test]
    fn test_log_buffer_overflow_multiple() {
        let mut buf = LogBuffer::new(2);
        buf.push("a".to_string());
        buf.push("b".to_string());
        buf.push("c".to_string());
        buf.push("d".to_string());
        buf.push("e".to_string());
        assert_eq!(buf.entries.len(), 2);
        assert_eq!(buf.entries[0], "d");
        assert_eq!(buf.entries[1], "e");
    }

    #[test]
    fn test_log_buffer_single_entry() {
        let mut buf = LogBuffer::new(1);
        buf.push("only".to_string());
        assert_eq!(buf.entries.len(), 1);
        assert_eq!(buf.entries[0], "only");
    }

    #[test]
    fn test_log_buffer_single_overflow() {
        let mut buf = LogBuffer::new(1);
        buf.push("first".to_string());
        buf.push("second".to_string());
        assert_eq!(buf.entries.len(), 1);
        assert_eq!(buf.entries[0], "second");
    }

    #[test]
    fn test_log_buffer_capacity_exact() {
        let mut buf = LogBuffer::new(5);
        for i in 0..5 {
            buf.push(format!("entry {}", i));
        }
        assert_eq!(buf.entries.len(), 5);
        assert_eq!(buf.entries[0], "entry 0");
        assert_eq!(buf.entries[4], "entry 4");
    }

    #[test]
    fn test_log_buffer_large_capacity() {
        let mut buf = LogBuffer::new(1000);
        for i in 0..100 {
            buf.push(format!("entry {}", i));
        }
        assert_eq!(buf.entries.len(), 100);
        assert_eq!(buf.entries[0], "entry 0");
        assert_eq!(buf.entries[99], "entry 99");
    }
}