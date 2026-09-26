use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq)]
pub enum ConversionStatus {
    Idle,
    Running { progress: f32 },
    Completed,
    Failed { error_log: String },
}

#[derive(Clone, Debug)]
pub struct ConversionState {
    pub status: ConversionStatus,
    pub ffmpeg_output: String,
    pub current_line: String,
}

impl ConversionState {
    pub fn idle() -> Self {
        ConversionState {
            status: ConversionStatus::Idle,
            ffmpeg_output: String::new(),
            current_line: String::new(),
        }
    }
}

pub type SharedConversionState = Arc<Mutex<ConversionState>>;

pub type CancelFlag = Arc<AtomicBool>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_conversion_state_idle_initial() {
        let s = ConversionState::idle();
        assert_eq!(s.status, ConversionStatus::Idle);
        assert!(s.ffmpeg_output.is_empty());
        assert!(s.current_line.is_empty());
    }
}