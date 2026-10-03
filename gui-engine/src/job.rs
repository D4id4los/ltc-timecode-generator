use std::any::Any;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use audio_core::LtcDecodeError;
use audio_core::LtcDetectionResult;
use log::{error, warn};

use crate::camera_meta::CameraInfo;
use crate::converter::FfmpegCapabilities;
use crate::ffprobe::VideoAudioProbe;
use crate::file_pattern::MatchedGroup;
use crate::offload::SdCardInfo;

// ── Core type aliases ───────────────────────────────────────────────────

pub type JobIdValue = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct JobId(pub JobIdValue);

// ── Job kind ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JobKind {
    Conversion,
    OffloadCopy,
    OffloadScan,
    LtcDecode,
    LtcGroupDecode,
    FolderScan,
    ClipProbe,
    VideoProbe,
    DurationProbe,
    FfmpegCapProbe,
}

// ── Job phase ───────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum JobPhase {
    Idle,
    Running,
    Indeterminate,
    Succeeded,
    Cancelled,
    Failed,
}

// ── Unit specification ──────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct UnitSpec {
    pub weight: f32,
    pub label: String,
}

// ── Unit state ──────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UnitState {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

// ── Progress tracking (worker writes; engine reads each tick) ──────────

#[derive(Clone)]
pub struct ProgressTracker(Arc<TrackerInner>);

struct TrackerInner {
    units: Mutex<Vec<UnitInner>>,
    message: Mutex<String>,
    log: Mutex<String>,
    indeterminate: AtomicBool,
    speed: AtomicU64, // bytes per second * 1000 (fixed-point for atomic)
}

struct UnitInner {
    weight: f32,
    label: String,
    fraction: std::sync::atomic::AtomicU32, // 0..1000 (fixed-point)
    state: Mutex<UnitState>,
    message: Mutex<String>,
}

impl ProgressTracker {
    pub fn new(units: impl IntoIterator<Item = UnitSpec>) -> Self {
        let units: Vec<UnitInner> = units
            .into_iter()
            .map(|u| UnitInner {
                weight: u.weight,
                label: u.label,
                fraction: std::sync::atomic::AtomicU32::new(0),
                state: Mutex::new(UnitState::Pending),
                message: Mutex::new(String::new()),
            })
            .collect();
        ProgressTracker(Arc::new(TrackerInner {
            units: Mutex::new(units),
            message: Mutex::new(String::new()),
            log: Mutex::new(String::new()),
            indeterminate: AtomicBool::new(false),
            speed: AtomicU64::new(0),
        }))
    }

    /// Grows the unit list to `len` units if it is currently shorter;
    /// never shrinks (worker threads hold `UnitProgress` index handles into
    /// the shared unit vec — truncation would strand them).
    pub fn grow_to(&self, len: usize) {
        let mut units = self.0.units.lock().unwrap();
        let old_len = units.len();
        if len > old_len {
            let weight = if old_len > 0 {
                units[0].weight
            } else {
                1.0 / len as f32
            };
            for i in old_len..len {
                units.push(UnitInner {
                    weight,
                    label: format!("Step {}", i + 1),
                    fraction: std::sync::atomic::AtomicU32::new(0),
                    state: Mutex::new(UnitState::Pending),
                    message: Mutex::new(String::new()),
                });
            }
        }
    }

    pub fn unit(&self, idx: usize) -> UnitProgress {
        UnitProgress {
            tracker: Arc::clone(&self.0),
            idx,
        }
    }

    pub fn set_message(&self, msg: impl Into<String>) {
        *self.0.message.lock().unwrap() = msg.into();
    }

    /// Append a line to the tracker's rolling log. The log rides along in
    /// every `ProgressSnapshot` (→ `JobStatus.log`) and is captured into the
    /// final `JobOutcome`, so failures carry their step context to the UI.
    /// Capped so a chatty job cannot grow the published snapshot.
    pub fn push_log(&self, line: impl AsRef<str>) {
        const MAX_LOG_BYTES: usize = 4 * 1024;
        let mut log = self.0.log.lock().unwrap();
        if log.len() + line.as_ref().len() + 1 > MAX_LOG_BYTES {
            let keep = MAX_LOG_BYTES.saturating_sub(line.as_ref().len() + 1);
            // Drop the oldest lines, snapping forward past a partial one.
            let cut = log[log.len().saturating_sub(keep)..]
                .find('\n')
                .map(|i| log.len() - keep + i + 1)
                .unwrap_or(log.len());
            log.drain(..cut);
        }
        if !log.is_empty() {
            log.push('\n');
        }
        log.push_str(line.as_ref());
    }

    pub fn set_indeterminate(&self, b: bool) {
        self.0.indeterminate.store(b, Ordering::Relaxed);
    }

    pub fn set_speed(&self, bytes_per_sec: f64) {
        self.0
            .speed
            .store((bytes_per_sec * 1000.0) as u64, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> ProgressSnapshot {
        let inner = &self.0;
        let units = inner.units.lock().unwrap();
        let total_weight: f32 = units.iter().map(|u| u.weight).sum();
        let mut fraction = 0.0f32;
        let mut unit_snapshots = Vec::with_capacity(units.len());
        for u in units.iter() {
            let frac = u.fraction.load(Ordering::Relaxed) as f32 / 1000.0;
            fraction += u.weight * frac;
            unit_snapshots.push(UnitSnapshot {
                label: u.label.clone(),
                message: u.message.lock().unwrap().clone(),
                fraction: frac.min(1.0),
                state: *u.state.lock().unwrap(),
            });
        }
        let overall = if total_weight > 0.0 {
            (fraction / total_weight).min(1.0)
        } else {
            0.0
        };
        let speed_raw = inner.speed.load(Ordering::Relaxed);
        ProgressSnapshot {
            phase: if inner.indeterminate.load(Ordering::Relaxed) {
                JobPhase::Indeterminate
            } else {
                JobPhase::Running
            },
            fraction: overall,
            message: inner.message.lock().unwrap().clone(),
            speed: if speed_raw > 0 {
                Some(speed_raw as f64 / 1000.0)
            } else {
                None
            },
            units: unit_snapshots,
            log: inner.log.lock().unwrap().clone(),
        }
    }
}

#[derive(Clone)]
pub struct UnitProgress {
    tracker: Arc<TrackerInner>,
    idx: usize,
}

impl UnitProgress {
    pub fn set_fraction(&self, f: f32) {
        let units = self.tracker.units.lock().unwrap();
        if let Some(u) = units.get(self.idx) {
            u.fraction
                .store((f.clamp(0.0, 1.0) * 1000.0) as u32, Ordering::Relaxed);
        }
    }

    pub fn set_label(&self, label: impl Into<String>) {
        let mut units = self.tracker.units.lock().unwrap();
        if let Some(u) = units.get_mut(self.idx) {
            u.label = label.into();
        }
    }

    pub fn set_message(&self, msg: impl Into<String>) {
        let units = self.tracker.units.lock().unwrap();
        if let Some(u) = units.get(self.idx) {
            *u.message.lock().unwrap() = msg.into();
        }
    }

    pub fn set_state(&self, s: UnitState) {
        let units = self.tracker.units.lock().unwrap();
        if let Some(u) = units.get(self.idx) {
            *u.state.lock().unwrap() = s;
        }
    }

    pub fn finish(&self) {
        let units = self.tracker.units.lock().unwrap();
        if let Some(u) = units.get(self.idx) {
            u.fraction.store(1000, Ordering::Relaxed);
            *u.state.lock().unwrap() = UnitState::Done;
        }
    }
}

// ── Progress snapshot (read by engine each tick) ────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct UnitSnapshot {
    pub label: String,
    pub message: String,
    pub fraction: f32,
    pub state: UnitState,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProgressSnapshot {
    pub phase: JobPhase,
    pub fraction: f32,
    pub message: String,
    pub speed: Option<f64>,
    pub units: Vec<UnitSnapshot>,
    pub log: String,
}

// ── JobStatus (published in snapshot) ───────────────────────────────────

/// Published status of one job kind: a `ProgressSnapshot` plus the final
/// error, if the job failed.  Progress data is accessed through the
/// delegate methods (`phase()`, `fraction()`, …).
#[derive(Clone, Debug, PartialEq)]
pub struct JobStatus {
    pub progress: ProgressSnapshot,
    pub error: Option<String>,
}

impl JobStatus {
    pub fn idle() -> Self {
        JobStatus {
            progress: ProgressSnapshot {
                phase: JobPhase::Idle,
                fraction: 0.0,
                message: String::new(),
                speed: None,
                units: Vec::new(),
                log: String::new(),
            },
            error: None,
        }
    }

    /// A Running status with zero progress and no units — the shape every
    /// job-spawning command handler publishes before the first poll().
    pub fn running(message: impl Into<String>) -> Self {
        JobStatus {
            progress: ProgressSnapshot {
                phase: JobPhase::Running,
                fraction: 0.0,
                message: message.into(),
                speed: None,
                units: Vec::new(),
                log: String::new(),
            },
            error: None,
        }
    }

    pub fn from_progress(snap: &ProgressSnapshot) -> Self {
        JobStatus {
            progress: snap.clone(),
            error: None,
        }
    }

    pub fn phase(&self) -> JobPhase {
        self.progress.phase
    }

    pub fn fraction(&self) -> f32 {
        self.progress.fraction
    }

    pub fn message(&self) -> &str {
        &self.progress.message
    }

    pub fn speed(&self) -> Option<f64> {
        self.progress.speed
    }

    pub fn units(&self) -> &[UnitSnapshot] {
        &self.progress.units
    }

    pub fn log(&self) -> &str {
        &self.progress.log
    }

    pub fn is_active(&self) -> bool {
        matches!(self.phase(), JobPhase::Running | JobPhase::Indeterminate)
    }

    pub fn apply_outcome(&mut self, outcome: &JobOutcome) {
        match outcome {
            JobOutcome::Succeeded { log, .. } => {
                self.progress.phase = JobPhase::Succeeded;
                if !log.is_empty() {
                    self.progress.log = log.clone();
                }
            }
            JobOutcome::Cancelled { log } => {
                self.progress.phase = JobPhase::Cancelled;
                if !log.is_empty() {
                    self.progress.log = log.clone();
                }
            }
            JobOutcome::Failed { error, log } => {
                self.progress.phase = JobPhase::Failed;
                self.error = Some(error.clone());
                if !log.is_empty() {
                    self.progress.log = log.clone();
                }
            }
        }
    }
}

// ── Cancellation ────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        CancelToken(Arc::new(AtomicBool::new(false)))
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub fn check(&self) -> Result<(), JobError> {
        if self.is_cancelled() {
            Err(JobError::Cancelled)
        } else {
            Ok(())
        }
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn inner(&self) -> &Arc<AtomicBool> {
        &self.0
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

// ── Job error ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum JobError {
    Cancelled,
    Failed(String),
}

impl std::fmt::Display for JobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobError::Cancelled => write!(f, "cancelled"),
            JobError::Failed(msg) => write!(f, "{}", msg),
        }
    }
}

// ── Job outcome ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum JobOutcome {
    Succeeded { log: String },
    Cancelled { log: String },
    Failed { error: String, log: String },
}

// ── Payload enums ───────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub enum JobItem {
    DurationResult {
        path: PathBuf,
        secs: Option<f64>,
    },
    ClipLtcResult {
        index: usize,
        /// Boxed to keep this variant's size close to `DurationResult`.
        result: Result<Box<LtcDetectionResult>, String>,
    },
}

#[derive(Clone, Debug)]
pub enum JobFinal {
    Conversion {
        encoder_used: Option<String>,
        steps_attempted: usize,
    },
    OffloadCopy {
        completed_devices: Vec<String>,
    },
    OffloadScan {
        cards: Vec<SdCardInfo>,
    },
    FolderScan {
        path: PathBuf,
        groups: Vec<MatchedGroup>,
    },
    Decode {
        result: Result<LtcDetectionResult, LtcDecodeError>,
        path: PathBuf,
    },
    VideoProbe {
        result: Result<VideoAudioProbe, String>,
    },
    ClipProbes {
        probes: Vec<Result<VideoAudioProbe, String>>,
        cameras: Vec<Option<CameraInfo>>,
        device_name: Option<String>,
    },
    DurationsDone,
    FfmpegCaps {
        caps: Option<FfmpegCapabilities>,
    },
    NoPayload,
}

// ── Events ──────────────────────────────────────────────────────────────

// JobFinal is deliberately unboxed: events are short-lived on an internal
// channel, and boxing would force a deref-match rewrite of every handler.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum JobEvent {
    Item {
        job: JobId,
        kind: JobKind,
        item: JobItem,
    },
    Finished {
        job: JobId,
        kind: JobKind,
        outcome: JobOutcome,
        payload: JobFinal,
    },
}

// ── Job context (passed to the worker closure) ──────────────────────────

pub struct JobContext {
    pub progress: ProgressTracker,
    pub cancel: CancelToken,
    pub emit: Box<dyn Fn(JobItem) + Send + Sync>,
}

impl JobContext {
    pub fn emit(&self, item: JobItem) {
        (self.emit)(item);
    }
}

// ── Job spec ────────────────────────────────────────────────────────────

pub struct JobSpec<'a> {
    pub kind: JobKind,
    pub name: &'a str,
    pub units: Vec<UnitSpec>,
}

// ── Active job tracking ─────────────────────────────────────────────────

struct ActiveJob {
    id: JobId,
    kind: JobKind,
    tracker: ProgressTracker,
    cancel: CancelToken,
    handle: Option<JoinHandle<()>>,
}

// ── Job supervisor ──────────────────────────────────────────────────────

pub struct JobSupervisor {
    next_id: JobIdValue,
    tx: Sender<JobEvent>,
    rx: Receiver<JobEvent>,
    active: Vec<ActiveJob>,
    events_buffer: Vec<JobEvent>,
    /// Tracks the most-recently-spawned JobId per kind. Used by the engine
    /// drain loop to reject stale Item/Finished events from superseded jobs.
    pub latest_job: HashMap<JobKind, JobId>,
}

impl JobSupervisor {
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        JobSupervisor {
            next_id: 1,
            tx,
            rx,
            active: Vec::new(),
            events_buffer: Vec::new(),
            latest_job: HashMap::new(),
        }
    }

    pub fn is_running(&self, kind: JobKind) -> bool {
        self.active.iter().any(|j| j.kind == kind)
    }

    /// Cancel the running job of the given kind. Returns true if a job was cancelled.
    pub fn cancel(&self, kind: JobKind) -> bool {
        for job in &self.active {
            if job.kind == kind {
                job.cancel.cancel();
                return true;
            }
        }
        false
    }

    /// Poll all active jobs and return their progress snapshots.
    /// Removes finished jobs (handles that have completed).
    pub fn poll(&mut self) -> Vec<(JobId, JobKind, ProgressSnapshot)> {
        // Drain events from the channel
        loop {
            match self.rx.try_recv() {
                Ok(event) => self.events_buffer.push(event),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    warn!("Job event channel disconnected");
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
            }
        }

        // Check for finished threads and collect progress
        let mut results = Vec::new();
        let mut finished_indices: Vec<usize> = Vec::new();

        for job in self.active.iter() {
            let snapshot = job.tracker.snapshot();
            results.push((job.id, job.kind, snapshot));
        }

        for i in (0..self.active.len()).rev() {
            if let Some(ref handle) = self.active[i].handle {
                if handle.is_finished() {
                    finished_indices.push(i);
                }
            }
        }

        for &idx in &finished_indices {
            let mut job = self.active.swap_remove(idx);
            if let Some(handle) = job.handle.take() {
                let _ = handle.join();
            }
        }

        results
    }

    /// Drain accumulated events from the events buffer.
    pub fn drain(&mut self) -> Vec<JobEvent> {
        std::mem::take(&mut self.events_buffer)
    }

    /// Shutdown all active jobs and join their threads with a timeout.
    pub fn shutdown(self, timeout: Duration) {
        // Cancel all active jobs
        for job in &self.active {
            job.cancel.cancel();
        }

        let deadline = Instant::now() + timeout;
        for mut job in self.active {
            if let Some(handle) = job.handle.take() {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining > Duration::from_millis(10) {
                    let _ = handle.join();
                }
            }
        }
    }

    /// Cancel all active jobs.
    pub fn cancel_all(&self) {
        for job in &self.active {
            job.cancel.cancel();
        }
    }
}

impl Default for JobSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

// ── spawn_job ───────────────────────────────────────────────────────────

pub fn spawn_job<T, F>(sup: &mut JobSupervisor, spec: JobSpec, f: F)
where
    F: FnOnce(&JobContext) -> Result<T, JobError> + Send + 'static,
    T: Into<JobFinal> + Send + 'static,
{
    let id = JobId(sup.next_id);
    sup.next_id += 1;
    sup.latest_job.insert(spec.kind, id);

    let tracker = ProgressTracker::new(spec.units);
    let cancel = CancelToken::new();
    let event_tx = sup.tx.clone();
    let kind = spec.kind;
    let job_name = spec.name.to_string();
    let tracker_clone = tracker.clone();
    let cancel_clone = cancel.clone();

    let emit_tx = sup.tx.clone();
    let emit: Box<dyn Fn(JobItem) + Send + Sync> = Box::new(move |item| {
        let _ = emit_tx.send(JobEvent::Item {
            job: id,
            kind,
            item,
        });
    });

    let ctx = JobContext {
        progress: tracker_clone,
        cancel: cancel_clone,
        emit,
    };

    let handle = std::thread::Builder::new()
        .name(job_name.clone())
        .spawn(move || {
            let (outcome, payload) = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                f(&ctx)
            })) {
                Ok(Ok(result)) => {
                    let payload: JobFinal = result.into();
                    (JobOutcome::Succeeded {
                        log: String::new(),
                    }, payload)
                }
                Ok(Err(JobError::Cancelled)) => (JobOutcome::Cancelled {
                    log: String::new(),
                }, JobFinal::NoPayload),
                Ok(Err(JobError::Failed(msg))) => (JobOutcome::Failed {
                    error: msg,
                    log: String::new(),
                }, JobFinal::NoPayload),
                Err(panic) => {
                    let msg = panic_message(&panic);
                    error!("Job '{}' (id={:?}) panicked: {}", job_name, id, msg);
                    (JobOutcome::Failed {
                        error: format!("internal error (panic in {})", job_name),
                        log: msg,
                    }, JobFinal::NoPayload)
                }
            };

            let _ = event_tx.send(JobEvent::Finished {
                job: id,
                kind,
                outcome,
                payload,
            });
        })
        .expect("failed to spawn job thread");

    sup.active.push(ActiveJob {
        id,
        kind: spec.kind,
        tracker,
        cancel,
        handle: Some(handle),
    });
}

fn panic_message(p: &Box<dyn Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

// ── Speed meter ─────────────────────────────────────────────────────────

/// EMA-smoothed throughput meter. Tracks byte/block deltas between polls
/// and computes a smoothed speed with stall decay.
pub struct SpeedMeter {
    last_value: usize,
    last_time: Option<Instant>,
    last_progress: Option<Instant>,
    smoothed: f64,
}

impl SpeedMeter {
    pub fn new() -> Self {
        SpeedMeter {
            last_value: 0,
            last_time: None,
            last_progress: None,
            smoothed: 0.0,
        }
    }

    /// Feed a new cumulative block/byte count and the current instant.
    /// Returns the smoothed speed in the same units per second.
    pub fn update(&mut self, value: usize, now: Instant) -> f64 {
        let delta_v = value.saturating_sub(self.last_value);
        self.last_value = value;

        if delta_v > 0 {
            self.last_progress = Some(now);
        }

        let since_progress = match self.last_progress {
            Some(t) => now.saturating_duration_since(t).as_secs_f64(),
            None => 0.0,
        };

        if let Some(last_time) = self.last_time {
            let delta_t = now.saturating_duration_since(last_time).as_secs_f64();
            if delta_t > 0.001 {
                if delta_v > 0 {
                    let instant_speed = delta_v as f64 / delta_t;
                    self.smoothed = 0.7 * self.smoothed + 0.3 * instant_speed;
                } else if since_progress > 2.0 {
                    self.smoothed *= 0.5;
                }
            }
        } else {
            self.smoothed = 0.0;
        }

        self.last_time = Some(now);
        self.smoothed
    }

    pub fn reset(&mut self) {
        self.last_value = 0;
        self.last_time = None;
        self.last_progress = None;
        self.smoothed = 0.0;
    }

    pub fn current(&self) -> f64 {
        self.smoothed
    }
}

impl Default for SpeedMeter {
    fn default() -> Self {
        Self::new()
    }
}

// ── Unit tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use std::time::Instant;

    /// Poll the supervisor until the job of `kind` has finished (worker thread
    /// reaped by `poll()`), then return all accumulated events.
    ///
    /// Deterministic: the worker sends `JobEvent::Finished` just before it
    /// exits, so once `is_running(kind)` is false the terminal event is
    /// guaranteed to be in the drain buffer.
    fn wait_for_finished(
        sup: &mut JobSupervisor,
        kind: JobKind,
        timeout: Duration,
    ) -> Vec<JobEvent> {
        let deadline = Instant::now() + timeout;
        loop {
            sup.poll();
            if !sup.is_running(kind) {
                return sup.drain();
            }
            assert!(
                Instant::now() < deadline,
                "job {kind:?} still running after {timeout:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn job_status_running_has_running_phase_and_message() {
        let st = JobStatus::running("Decoding LTC group: 0/3 clips");
        assert_eq!(st.phase(), JobPhase::Running);
        assert_eq!(st.fraction(), 0.0);
        assert_eq!(st.message(), "Decoding LTC group: 0/3 clips");
        assert_eq!(st.error, None);
        assert!(st.units().is_empty());
        assert_ne!(st, JobStatus::idle());
    }

    #[test]
    fn push_log_appends_lines() {
        let t = ProgressTracker::new(Vec::<UnitSpec>::new());
        t.push_log("step a failed: boom");
        t.push_log("step b ok");
        assert_eq!(t.snapshot().log, "step a failed: boom\nstep b ok");
    }

    #[test]
    fn push_log_is_capped() {
        let t = ProgressTracker::new(Vec::<UnitSpec>::new());
        for i in 0..200 {
            t.push_log(format!("line-{:03} 012345678901234567890123456789", i));
        }
        let log = t.snapshot().log;
        assert!(log.len() <= 4 * 1024, "log grew to {} bytes", log.len());
        // Only the newest lines survive the cap.
        assert!(!log.contains("line-000"));
        assert!(log.contains("line-199"));
    }

    fn empty_spec() -> JobSpec<'static> {
        JobSpec {
            kind: JobKind::FfmpegCapProbe,
            name: "test",
            units: Vec::new(),
        }
    }

    // ── ProgressTracker: weighted fraction & resize ──────────────────────

    #[test]
    fn test_progress_tracker_weighted_fraction() {
        let units = vec![
            UnitSpec {
                weight: 0.3,
                label: "A".into(),
            },
            UnitSpec {
                weight: 0.7,
                label: "B".into(),
            },
        ];
        let pt = ProgressTracker::new(units);
        let snap = pt.snapshot();
        assert!((snap.fraction - 0.0).abs() < 0.001);

        pt.unit(0).set_fraction(1.0);
        let snap = pt.snapshot();
        assert!((snap.fraction - 0.3).abs() < 0.001);

        pt.unit(1).set_fraction(1.0);
        let snap = pt.snapshot();
        assert!((snap.fraction - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_progress_tracker_grow_to() {
        let pt = ProgressTracker::new(vec![UnitSpec {
            weight: 1.0,
            label: "initial".into(),
        }]);
        assert_eq!(pt.snapshot().units.len(), 1);

        pt.grow_to(3);
        assert_eq!(pt.snapshot().units.len(), 3);

        // grow_to(1) after growing: shrink is a no-op — worker threads hold
        // UnitProgress index handles into the shared unit vec, and
        // truncation would strand them. The grow-only API name is the
        // contract; no production call site ever shrinks.
        pt.grow_to(1);
        assert_eq!(pt.snapshot().units.len(), 3);
    }

    // ── Unit state transitions ──────────────────────────────────────────

    #[test]
    fn test_unit_state_transitions() {
        let pt = ProgressTracker::new(vec![UnitSpec {
            weight: 1.0,
            label: "X".into(),
        }]);
        let u = pt.unit(0);
        assert_eq!(u.idx, 0);

        let snap = pt.snapshot();
        assert_eq!(snap.units[0].state, UnitState::Pending);

        u.set_state(UnitState::Running);
        assert_eq!(pt.snapshot().units[0].state, UnitState::Running);

        u.set_state(UnitState::Done);
        assert_eq!(pt.snapshot().units[0].state, UnitState::Done);

        u.set_state(UnitState::Failed);
        assert_eq!(pt.snapshot().units[0].state, UnitState::Failed);

        u.set_state(UnitState::Skipped);
        assert_eq!(pt.snapshot().units[0].state, UnitState::Skipped);
    }

    // ── Snapshot clamping ───────────────────────────────────────────────

    #[test]
    fn test_snapshot_clamps_fraction() {
        let pt = ProgressTracker::new(vec![UnitSpec {
            weight: 1.0,
            label: "X".into(),
        }]);
        pt.unit(0).set_fraction(2.0);
        let snap = pt.snapshot();
        assert!((snap.fraction - 1.0).abs() < 0.001);
    }

    // ── CancelToken ─────────────────────────────────────────────────────

    #[test]
    fn test_cancel_token_default_not_cancelled() {
        let ct = CancelToken::new();
        assert!(!ct.is_cancelled());
        assert!(ct.check().is_ok());
    }

    #[test]
    fn test_cancel_token_cancel() {
        let ct = CancelToken::new();
        ct.cancel();
        assert!(ct.is_cancelled());
        assert!(ct.check().is_err());
    }

    #[test]
    fn test_cancel_token_clone_shares_state() {
        let ct = CancelToken::new();
        let ct2 = ct.clone();
        ct.cancel();
        assert!(ct2.is_cancelled());
    }

    // ── Panic → Failed outcome ─────────────────────────────────────────

    #[test]
    fn test_spawn_job_panic_converts_to_failed() {
        let mut sup = JobSupervisor::new();
        spawn_job::<JobFinal, _>(&mut sup, empty_spec(), |_ctx| -> Result<JobFinal, JobError> {
            panic!("deliberate panic");
        });

        let events = wait_for_finished(
            &mut sup,
            JobKind::FfmpegCapProbe,
            Duration::from_secs(10),
        );
        let finished = events.iter().find(|e| matches!(e, JobEvent::Finished { .. }));
        assert!(finished.is_some(), "expected a Finished event");
        if let Some(JobEvent::Finished { outcome, .. }) = finished {
            match outcome {
                JobOutcome::Failed { error, .. } => {
                    assert!(
                        error.contains("panic"),
                        "expected panic in error, got: {}",
                        error
                    );
                }
                other => panic!("expected Failed, got {:?}", other),
            }
        }
    }

    // ── Cancelled outcome ──────────────────────────────────────────────

    #[test]
    fn test_spawn_job_cancelled_check() {
        let mut sup = JobSupervisor::new();
        let cancel_outer = CancelToken::new();
        let cancel_clone = cancel_outer.clone();

        spawn_job::<JobFinal, _>(&mut sup, empty_spec(), move |ctx| -> Result<JobFinal, JobError> {
            // Signal cancellation from outside
            cancel_clone.cancel();
            ctx.cancel.check()?;
            Ok(JobFinal::NoPayload)
        });

        sup.cancel_all();

        let events = wait_for_finished(
            &mut sup,
            JobKind::FfmpegCapProbe,
            Duration::from_secs(10),
        );
        let finished = events.iter().find(|e| matches!(e, JobEvent::Finished { .. }));
        assert!(finished.is_some(), "expected a Finished event");
        if let Some(JobEvent::Finished { outcome, .. }) = finished {
            match outcome {
                JobOutcome::Cancelled { .. } => {}
                other => panic!("expected Cancelled, got {:?}", other),
            }
        }
    }

    // ── emit ordering ──────────────────────────────────────────────────

    #[test]
    fn test_spawn_job_emit_before_finished() {
        let mut sup = JobSupervisor::new();
        let path = PathBuf::from("/test/file.wav");

        spawn_job::<JobFinal, _>(&mut sup, empty_spec(), move |ctx| -> Result<JobFinal, JobError> {
            ctx.emit(JobItem::DurationResult {
                path: path.clone(),
                secs: Some(10.0),
            });
            Ok(JobFinal::DurationsDone)
        });

        let events: Vec<_> = wait_for_finished(
            &mut sup,
            JobKind::FfmpegCapProbe,
            Duration::from_secs(10),
        );
        // Emitted item and Finished should both appear
        let item_pos = events
            .iter()
            .position(|e| matches!(e, JobEvent::Item { item: JobItem::DurationResult { .. }, .. }));
        let fin_pos = events.iter().position(|e| matches!(e, JobEvent::Finished { .. }));
        assert!(item_pos.is_some(), "should have received an Item event");
        assert!(fin_pos.is_some(), "should have received a Finished event");
        // Same worker thread, same channel ⇒ Item strictly precedes Finished
        assert!(
            item_pos.unwrap() < fin_pos.unwrap(),
            "Item event must precede Finished event"
        );
    }

    // ── is_running guard ───────────────────────────────────────────────

    #[test]
    fn test_is_running_guard() {
        let mut sup = JobSupervisor::new();
        assert!(!sup.is_running(JobKind::FfmpegCapProbe));

        spawn_job::<JobFinal, _>(&mut sup, empty_spec(), |_ctx| -> Result<JobFinal, JobError> {
            std::thread::sleep(Duration::from_millis(100));
            Ok(JobFinal::NoPayload)
        });

        assert!(sup.is_running(JobKind::FfmpegCapProbe));
    }

    // ── Drain generation semantics ─────────────────────────────────────

    #[test]
    fn test_drain_returns_accumulated_events() {
        let mut sup = JobSupervisor::new();
        spawn_job::<JobFinal, _>(&mut sup, empty_spec(), |_ctx| -> Result<JobFinal, JobError> {
            std::thread::sleep(Duration::from_millis(10));
            Ok(JobFinal::NoPayload)
        });

        // Wait for job to finish
        let events = wait_for_finished(
            &mut sup,
            JobKind::FfmpegCapProbe,
            Duration::from_secs(10),
        );
        assert!(!events.is_empty(), "should have events after drain");

        // Second drain should be empty
        let events2 = sup.drain();
        assert!(events2.is_empty(), "second drain should be empty");
    }

    // ── SpeedMeter ─────────────────────────────────────────────────────

    #[test]
    fn test_speed_meter_initial_zero() {
        let sm = SpeedMeter::new();
        assert!((sm.current() - 0.0).abs() < 1e-6);
    }

    #[test]
    fn test_speed_meter_positive_delta() {
        let mut sm = SpeedMeter::new();
        let now = Instant::now();
        // First update sets baseline
        let s = sm.update(0, now);
        assert!((s - 0.0).abs() < 1e-6);

        // Second update with positive delta
        let later = now + Duration::from_secs(1);
        let s = sm.update(1000, later);
        assert!(s > 0.0, "speed should be positive, got {}", s);
    }

    #[test]
    fn test_speed_meter_stall_decay() {
        let mut sm = SpeedMeter::new();
        let now = Instant::now();
        sm.update(0, now);
        let later = now + Duration::from_secs(1);
        sm.update(100, later);

        let speed_after_progress = sm.current();
        assert!(speed_after_progress > 0.0);

        // Stall longer than 2s
        let stall = later + Duration::from_secs(3);
        sm.update(100, stall);
        assert!(
            sm.current() < speed_after_progress,
            "speed should decay during stall"
        );
    }

    #[test]
    fn test_speed_meter_reset() {
        let mut sm = SpeedMeter::new();
        let now = Instant::now();
        sm.update(0, now);
        sm.update(100, now + Duration::from_secs(1));
        sm.reset();
        assert!((sm.current() - 0.0).abs() < 1e-6);
        assert!(sm.last_time.is_none());
    }

    #[test]
    fn test_speed_meter_short_stall_holds_value() {
        let mut sm = SpeedMeter::new();
        let now = Instant::now();
        sm.update(0, now);
        sm.update(100, now + Duration::from_millis(500));
        let speed = sm.current();
        assert!(speed > 0.0);

        // Short stall (< 2s) should hold value
        sm.update(100, now + Duration::from_secs(1));
        assert!(
            (sm.current() - speed).abs() < 0.001,
            "short stall should hold value"
        );
    }

    // ── ProgressTracker: message & indeterminate ───────────────────────

    #[test]
    fn test_progress_tracker_set_message() {
        let pt = ProgressTracker::new(vec![UnitSpec {
            weight: 1.0,
            label: "X".into(),
        }]);
        pt.set_message("working");
        assert_eq!(pt.snapshot().message, "working");
    }

    #[test]
    fn test_progress_tracker_indeterminate() {
        let pt = ProgressTracker::new(Vec::<UnitSpec>::new());
        pt.set_indeterminate(true);
        assert_eq!(pt.snapshot().phase, JobPhase::Indeterminate);
    }

    #[test]
    fn test_progress_tracker_unit_message() {
        let pt = ProgressTracker::new(vec![UnitSpec {
            weight: 1.0,
            label: "X".into(),
        }]);
        pt.unit(0).set_message("processing file");
        assert_eq!(pt.snapshot().units[0].message, "processing file");
    }

    // ── UnitProgress finish ────────────────────────────────────────────

    #[test]
    fn test_unit_progress_finish() {
        let pt = ProgressTracker::new(vec![UnitSpec {
            weight: 1.0,
            label: "X".into(),
        }]);
        pt.unit(0).finish();
        let snap = pt.snapshot();
        assert!((snap.fraction - 1.0).abs() < 0.001);
        assert_eq!(snap.units[0].state, UnitState::Done);
    }

    // ── ProgressTracker set_speed ──────────────────────────────────────

    #[test]
    fn test_progress_tracker_speed() {
        let pt = ProgressTracker::new(Vec::<UnitSpec>::new());
        assert!(pt.snapshot().speed.is_none());
        pt.set_speed(1_500_000.0);
        let snap = pt.snapshot();
        assert!(snap.speed.is_some());
        assert!((snap.speed.unwrap() - 1_500_000.0).abs() < 1.0);
    }

    // ── Cancel via supervisor ──────────────────────────────────────────

    #[test]
    fn test_supervisor_cancel_by_kind() {
        let mut sup = JobSupervisor::new();
        spawn_job::<JobFinal, _>(&mut sup, empty_spec(), |ctx| {
            while !ctx.cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(JobError::Cancelled)
        });

        assert!(sup.is_running(JobKind::FfmpegCapProbe));
        assert!(sup.cancel(JobKind::FfmpegCapProbe));

        let events = wait_for_finished(
            &mut sup,
            JobKind::FfmpegCapProbe,
            Duration::from_secs(10),
        );
        let cancelled = events.iter().any(|e| matches!(e, JobEvent::Finished { outcome: JobOutcome::Cancelled { .. }, .. }));
        assert!(cancelled, "expected Cancelled outcome");
    }

    // ── Shutdown cancels all ───────────────────────────────────────────

    #[test]
    fn test_supervisor_shutdown_joins() {
        let mut sup = JobSupervisor::new();

        let flag = Arc::new(AtomicBool::new(false));
        let flag_clone = Arc::clone(&flag);

        spawn_job::<JobFinal, _>(&mut sup, empty_spec(), move |ctx| {
            while !ctx.cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(10));
            }
            flag_clone.store(true, Ordering::Relaxed);
            Err(JobError::Cancelled)
        });

        sup.shutdown(Duration::from_secs(5));
        assert!(flag.load(Ordering::Relaxed), "job should have been cancelled");
    }

    // ── JobStatus::is_active ──────────────────────────────────────────────

    fn can_start_offload(cards_empty: bool, has_parent: bool, copy: &JobStatus, scan: &JobStatus) -> bool {
        !cards_empty && has_parent && !copy.is_active() && !scan.is_active()
    }

    #[test]
    fn test_can_start_offload_blocks_on_indeterminate() {
        let mut copy = JobStatus::idle();
        let mut scan = JobStatus::idle();

        assert!(can_start_offload(false, true, &copy, &scan),
            "should start when both jobs idle");

        copy.progress.phase = JobPhase::Indeterminate;
        assert!(!can_start_offload(false, true, &copy, &scan),
            "should not start when copy is Indeterminate");

        copy.progress.phase = JobPhase::Idle;
        scan.progress.phase = JobPhase::Indeterminate;
        assert!(!can_start_offload(false, true, &copy, &scan),
            "should not start when scan is Indeterminate");

        copy.progress.phase = JobPhase::Running;
        scan.progress.phase = JobPhase::Running;
        assert!(!can_start_offload(false, true, &copy, &scan),
            "should not start when both are Running");
    }

    fn probe_status_label(probe_active: bool, probe_is_none: bool) -> &'static str {
        if probe_active {
            "Probing clip audio…"
        } else if probe_is_none {
            "Clip audio probe failed."
        } else {
            "No channels to map."
        }
    }

    #[test]
    fn test_probe_label_shows_probing_during_indeterminate() {
        assert_eq!(probe_status_label(true, true), "Probing clip audio…",
            "Indeterminate/Running → show Probing");
        assert_eq!(probe_status_label(true, false), "Probing clip audio…");
        assert_eq!(probe_status_label(false, true), "Clip audio probe failed.",
            "Idle/Succeeded/Failed + no probe → show failed");
        assert_eq!(probe_status_label(false, false), "No channels to map.",
            "Probe exists → show no channels (unreachable in this branch)");
    }

    fn probe_is_loading(probe_job: &JobStatus) -> bool {
        probe_job.is_active()
    }

    fn probe_has_failed(probe_job: &JobStatus, ltc_probe_is_none: bool) -> bool {
        ltc_probe_is_none && !probe_job.is_active()
    }

    #[test]
    fn test_probe_loading_and_failed_with_indeterminate() {
        let mut probe = JobStatus::idle();

        assert!(!probe_is_loading(&probe), "Idle → not loading");
        assert!(probe_has_failed(&probe, true), "Idle + none → failed");

        probe.progress.phase = JobPhase::Indeterminate;
        assert!(probe_is_loading(&probe), "Indeterminate → loading");
        assert!(!probe_has_failed(&probe, true), "Indeterminate + none → not failed");

        probe.progress.phase = JobPhase::Running;
        assert!(probe_is_loading(&probe), "Running → loading");
        assert!(!probe_has_failed(&probe, true), "Running + none → not failed");

        probe.progress.phase = JobPhase::Succeeded;
        assert!(!probe_is_loading(&probe), "Succeeded → not loading");
    }

    // ── set_label ────────────────────────────────────────────────────────

    #[test]
    fn test_unit_progress_set_label() {
        let pt = ProgressTracker::new(vec![UnitSpec {
            weight: 1.0,
            label: "initial".into(),
        }]);
        pt.unit(0).set_label("updated label");
        let snap = pt.snapshot();
        assert_eq!(snap.units[0].label, "updated label");
    }

    // ── Supervisor poll shows worker progress ────────────────────────────

    #[test]
    fn test_supervisor_poll_shows_worker_progress() {
        let mut sup = JobSupervisor::new();
        let spec = JobSpec {
            kind: JobKind::LtcDecode,
            name: "progress-test",
            units: vec![UnitSpec { weight: 1.0, label: "phase1".into() }],
        };

        spawn_job::<JobFinal, _>(&mut sup, spec, |ctx| -> Result<JobFinal, JobError> {
            ctx.progress.unit(0).set_fraction(0.5);
            ctx.progress.set_message("halfway");
            std::thread::sleep(Duration::from_millis(200));
            Ok(JobFinal::NoPayload)
        });

        let deadline = Instant::now() + Duration::from_secs(30);
        let observed: bool = loop {
            // Drain any finished events first so they don't accumulate
            let _ = sup.drain();
            let snapshots = sup.poll();
            if let Some((_, _, snap)) = snapshots.first() {
                if (snap.fraction - 0.5).abs() < 0.001 {
                    break true;
                }
            }
            if Instant::now() > deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(20));
        };

        assert!(observed, "expected to observe fraction=0.5 in poll snapshot");
    }

    // ── latest_job tracking ─────────────────────────────────────────────

    #[test]
    fn test_spawn_job_updates_latest_job() {
        let mut sup = JobSupervisor::new();
        assert_eq!(sup.latest_job.get(&JobKind::FfmpegCapProbe), None);

        spawn_job::<JobFinal, _>(&mut sup, empty_spec(), |_ctx| -> Result<JobFinal, JobError> {
            std::thread::sleep(Duration::from_millis(10));
            Ok(JobFinal::NoPayload)
        });

        let id = sup.latest_job.get(&JobKind::FfmpegCapProbe);
        assert!(id.is_some(), "latest_job should be set after spawn");

        // Spawning again of the same kind updates the ID
        let first_id = *id.unwrap();
        spawn_job::<JobFinal, _>(&mut sup, empty_spec(), |_ctx| -> Result<JobFinal, JobError> {
            std::thread::sleep(Duration::from_millis(10));
            Ok(JobFinal::NoPayload)
        });

        let second_id = sup.latest_job.get(&JobKind::FfmpegCapProbe).unwrap();
        assert!(second_id.0 > first_id.0, "second spawn should have larger JobId");
    }

    // NOTE: stale-event *gating* lives in the engine's event-drain loop
    // (`engine.rs::job_event_is_stale`, unit-tested there); the supervisor's
    // `drain()` returns everything unfiltered. This test therefore only
    // proves that re-running a job of the same kind delivers exactly one
    // Finished event.
    #[test]
    fn test_same_kind_rerun_delivers_one_finished_event() {
        let mut sup = JobSupervisor::new();

        // Spawn a quick job that finishes immediately
        spawn_job::<JobFinal, _>(&mut sup, empty_spec(), |_ctx| -> Result<JobFinal, JobError> {
            Ok(JobFinal::NoPayload)
        });

        // The Finished event is in the buffer — drain it so we start clean
        let first_events = wait_for_finished(
            &mut sup,
            JobKind::FfmpegCapProbe,
            Duration::from_secs(10),
        );
        assert!(
            first_events.iter().any(|e| matches!(e, JobEvent::Finished { .. })),
            "first job should have finished"
        );

        // Spawn a second job of same kind — updates latest_job to new ID
        spawn_job::<JobFinal, _>(&mut sup, empty_spec(), |_ctx| -> Result<JobFinal, JobError> {
            Ok(JobFinal::NoPayload)
        });

        let second_events = wait_for_finished(
            &mut sup,
            JobKind::FfmpegCapProbe,
            Duration::from_secs(10),
        );

        // Only events from the second (latest) job should appear;
        // the first job's Finished event was already drained above so it's
        // irrelevant — what matters is that the second job's Finished event
        // carries the correct ID.
        let second_finished: Vec<&JobEvent> = second_events.iter()
            .filter(|e| matches!(e, JobEvent::Finished { .. }))
            .collect();
        assert_eq!(second_finished.len(), 1,
            "expected exactly one Finished event from second job, got {}",
            second_finished.len(),
        );
    }

    #[test]
    fn test_job_status_is_active() {
        let mut status = JobStatus::idle();
        assert!(!status.is_active(), "Idle should not be active");

        status.progress.phase = JobPhase::Running;
        assert!(status.is_active(), "Running should be active");

        status.progress.phase = JobPhase::Indeterminate;
        assert!(status.is_active(), "Indeterminate should be active");

        status.progress.phase = JobPhase::Succeeded;
        assert!(!status.is_active(), "Succeeded should not be active");

        status.progress.phase = JobPhase::Cancelled;
        assert!(!status.is_active(), "Cancelled should not be active");

        status.progress.phase = JobPhase::Failed;
        assert!(!status.is_active(), "Failed should not be active");
    }
}