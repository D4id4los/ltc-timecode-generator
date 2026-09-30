#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq)]
pub enum ConversionStatus {
    Idle,
    Running { progress: f32 },
    Completed,
    Failed { error_log: String },
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct ConversionState {
    pub status: ConversionStatus,
    pub ffmpeg_output: String,
    pub current_line: String,
}

#[allow(dead_code)]
impl ConversionState {
    pub fn idle() -> Self {
        ConversionState {
            status: ConversionStatus::Idle,
            ffmpeg_output: String::new(),
            current_line: String::new(),
        }
    }
}

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