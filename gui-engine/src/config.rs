use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use audio_core::Timecode;

pub(crate) const CONFIG_DIR: &str = "ltc-timecode-generator";
const CONFIG_FILE: &str = "converter_config.json";
const SESSIONS_DIR: &str = "sessions";

// ── Recording sessions (one JSON file per session) ──────────────────────

/// Schema tag written into every session file; unknown fields are ignored
/// on load (additive, versioned schema).
pub const SESSION_SCHEMA: &str = "ltc-session/v1";

/// Default continue gap in seconds (applied when loading a session:
/// new TC = last emitted + gap).
pub const DEFAULT_CONTINUE_GAP_SECONDS: u64 = 60;

/// Upper bound for the continue gap — a full day; anything larger is a
/// typo, not a shoot plan.
pub const MAX_CONTINUE_GAP_SECONDS: u64 = 86_400;

/// A persisted recording session: named bundle of the timecode
/// continuity state (fps, drop-frame, start TC, last emitted TC),
/// the continue gap, and the offload destinations the session's media
/// was copied to.
///
/// Stored as `<config_base_dir>/ltc-timecode-generator/sessions/<id>.json`,
/// written only by the engine thread (atomic temp+rename).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SessionRecord {
    pub schema: String,
    pub id: String,
    pub name: String,
    /// Creation date (ISO-8601, local) — also the id's date prefix.
    pub created_date: String,
    /// Last day the session was created/loaded (ISO-8601, local).
    pub last_active_date: String,
    pub fps: f64,
    pub drop_frame: bool,
    pub start_timecode: Timecode,
    /// Last timecode emitted (persisted on stop, on shutdown, and
    /// periodically while playing). Loading the session resumes at
    /// `last_emitted + continue_gap`.
    pub last_emitted_timecode: Timecode,
    #[serde(default = "default_continue_gap_seconds")]
    pub continue_gap_seconds: u64,
    #[serde(default)]
    pub offload_destinations: Vec<String>,
}

fn default_continue_gap_seconds() -> u64 {
    DEFAULT_CONTINUE_GAP_SECONDS
}

impl SessionRecord {
    /// Fresh session: `last_emitted` starts at `start_timecode`, the gap
    /// at [`DEFAULT_CONTINUE_GAP_SECONDS`], no offload destinations.
    pub fn new(
        id: String,
        name: String,
        created_date: String,
        fps: f64,
        drop_frame: bool,
        start_timecode: Timecode,
    ) -> Self {
        SessionRecord {
            schema: SESSION_SCHEMA.to_string(),
            created_date: created_date.clone(),
            last_active_date: created_date,
            fps,
            drop_frame,
            start_timecode,
            last_emitted_timecode: start_timecode,
            continue_gap_seconds: DEFAULT_CONTINUE_GAP_SECONDS,
            offload_destinations: Vec::new(),
            id,
            name,
        }
    }

    /// The continue-preview timecode shown before loading: last emitted
    /// plus the gap (DF-aware).
    pub fn continue_timecode(&self) -> Timecode {
        crate::timecode::advance_timecode_by_seconds(
            self.last_emitted_timecode,
            self.continue_gap_seconds,
            self.fps,
            self.drop_frame,
        )
    }
}

/// Typed error of the session store (Error-Handling R1/R4/R5).
#[derive(Debug)]
pub enum SessionStoreError {
    /// `config_base_dir()` did not resolve (no config dir available).
    NoConfigDir,
    /// Underlying filesystem/serialization failure.
    Io(String),
}

impl fmt::Display for SessionStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionStoreError::NoConfigDir => {
                write!(f, "session store: config directory unavailable")
            }
            SessionStoreError::Io(msg) => write!(f, "session store: {}", msg),
        }
    }
}

impl std::error::Error for SessionStoreError {}

/// Kebab-case slug of a session name: lowercased ASCII
/// alphanumeric, everything else collapsed to single `-`, trimmed.
/// Empty/undistinct names slugify to `"session"`.
pub fn slugify_session_name(name: &str) -> String {
    let mut slug = String::new();
    let mut prev_dash = true; // leading dashes trimmed
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("session");
    }
    slug
}

/// Session id: `YYYY-MM-DD-<kebab-slug>`, `-2`/`-3` suffix on collision
/// with `existing` ids (mirrors the offload collision naming).
pub fn make_session_id(name: &str, date: &str, existing: &[String]) -> String {
    let base = format!("{}-{}", date, slugify_session_name(name));
    if !existing.iter().any(|e| e == &base) {
        return base;
    }
    for n in 2.. {
        let candidate = format!("{}-{}", base, n);
        if !existing.iter().any(|e| e == &candidate) {
            return candidate;
        }
    }
    unreachable!("collision loop always terminates");
}

/// Directory holding the per-session JSON files.
pub fn sessions_dir() -> Option<PathBuf> {
    Some(sessions_dir_from_base(config_base_dir()?.as_path()))
}

fn sessions_dir_from_base(base: &Path) -> PathBuf {
    base.join(CONFIG_DIR).join(SESSIONS_DIR)
}

/// Load all persisted sessions, newest-active first. Corrupt files are
/// skipped with a loud warning (never fatal — one bad file must not
/// hide the rest of the shoot history).
pub fn load_sessions() -> Vec<SessionRecord> {
    match sessions_dir() {
        Some(dir) => load_sessions_from(&dir),
        None => Vec::new(),
    }
}

pub fn load_sessions_from(dir: &Path) -> Vec<SessionRecord> {
    let mut sessions = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return sessions;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|c| serde_json::from_str::<SessionRecord>(&c).map_err(|e| e.to_string()))
        {
            Ok(rec) => sessions.push(rec),
            Err(e) => {
                eprintln!(
                    "--- WARNING: skipping corrupt session file {}: {}",
                    path.display(),
                    e
                );
            }
        }
    }
    sessions.sort_by(|a, b| {
        b.last_active_date
            .cmp(&a.last_active_date)
            .then_with(|| a.id.cmp(&b.id))
    });
    sessions
}

/// Persist a session (atomic: temp file + rename in the sessions dir).
pub fn save_session(rec: &SessionRecord) -> Result<(), SessionStoreError> {
    let dir = sessions_dir().ok_or(SessionStoreError::NoConfigDir)?;
    save_session_to(&dir, rec)
}

pub fn save_session_to(dir: &Path, rec: &SessionRecord) -> Result<(), SessionStoreError> {
    fs::create_dir_all(dir).map_err(|e| SessionStoreError::Io(e.to_string()))?;
    let content =
        serde_json::to_string_pretty(rec).map_err(|e| SessionStoreError::Io(e.to_string()))?;
    let final_path = dir.join(format!("{}.json", rec.id));
    let tmp_path = dir.join(format!("{}.json.tmp", rec.id));
    fs::write(&tmp_path, content).map_err(|e| SessionStoreError::Io(e.to_string()))?;
    fs::rename(&tmp_path, &final_path).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        SessionStoreError::Io(e.to_string())
    })?;
    Ok(())
}

/// Delete a session file by id. Returns whether a file was removed.
/// Store-level only — phase 3 ships no delete UI.
pub fn delete_session(id: &str) -> Result<bool, SessionStoreError> {
    let dir = sessions_dir().ok_or(SessionStoreError::NoConfigDir)?;
    delete_session_from(&dir, id)
}

pub fn delete_session_from(dir: &Path, id: &str) -> Result<bool, SessionStoreError> {
    // The id is also the file stem; reject anything that could traverse.
    if id.contains('/') || id.contains('\\') || id.contains("..") {
        return Ok(false);
    }
    let path = dir.join(format!("{}.json", id));
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(SessionStoreError::Io(e.to_string())),
    }
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct ConverterConfig {
    pub last_input_folder: Option<String>,
    pub last_offload_parent: Option<String>,
    /// Accessibility text scale in percent; `None` = pre-feature config.
    #[serde(default)]
    pub text_scale_percent: Option<u32>,
}

/// Base directory for all persisted config/cache state.
///
/// Honors an `LTC_CONFIG_HOME` env override before `dirs::config_dir()`.
/// The override exists because `XDG_CONFIG_HOME` alone is ignored by
/// `dirs` on Windows (`SHGetKnownFolderPath` is registry-based), so engine
/// tests set both variables to isolate config state on every platform; it
/// also serves portable installs.
pub fn config_base_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("LTC_CONFIG_HOME") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs::config_dir()
}

fn config_path() -> Option<PathBuf> {
    config_base_dir().map(|base| base.join(CONFIG_DIR).join(CONFIG_FILE))
}

pub fn load() -> ConverterConfig {
    match config_path() {
        Some(p) => load_from(&p),
        None => ConverterConfig::default(),
    }
}

pub fn load_from(path: &Path) -> ConverterConfig {
    match fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => ConverterConfig::default(),
    }
}

pub fn save(config: &ConverterConfig) {
    let path = match config_path() {
        Some(p) => p,
        None => return,
    };
    save_to(&path, config);
}

pub fn save_to(path: &Path, config: &ConverterConfig) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(content) = serde_json::to_string_pretty(config) {
        let _ = fs::write(path, content);
    }
}

pub fn save_input_folder(path: &Path) {
    let mut cfg = load();
    cfg.last_input_folder = Some(path.to_string_lossy().to_string());
    save(&cfg);
}

pub fn save_offload_parent(path: &Path) {
    let mut cfg = load();
    cfg.last_offload_parent = Some(path.to_string_lossy().to_string());
    save(&cfg);
}

pub fn save_text_scale(percent: u32) {
    let mut cfg = load();
    cfg.text_scale_percent = Some(percent);
    save(&cfg);
}

/// Seed engine snapshot fields from a loaded config.
pub fn seed_snapshot_from_config(
    snapshot: &mut crate::state::AppStateSnapshot,
    cfg: &ConverterConfig,
) {
    // Offload parent folder
    if let Some(ref folder) = cfg.last_offload_parent {
        let p = Path::new(folder);
        if p.exists() {
            snapshot.offload.parent_folder = Some(p.to_path_buf());
        }
    }
    // Accessibility text scale
    if let Some(percent) = cfg.text_scale_percent {
        snapshot.text_scale_percent = percent.clamp(50, 400);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn save_and_load_offload_parent_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg_path = dir.path().join("test_config.json");

        let original = ConverterConfig {
            last_input_folder: Some("/home/input".into()),
            last_offload_parent: None,
            text_scale_percent: None,
        };
        save_to(&cfg_path, &original);

        // Initial load — offload parent should be None
        let loaded = load_from(&cfg_path);
        assert_eq!(loaded.last_offload_parent, None);

        // Simulate save_offload_parent: load, mutate, save
        let mut updated = loaded;
        updated.last_offload_parent = Some("/media/cards".into());
        save_to(&cfg_path, &updated);

        let reloaded = load_from(&cfg_path);
        assert_eq!(reloaded.last_offload_parent, Some("/media/cards".into()));
        // Unrelated fields preserved
        assert_eq!(reloaded.last_input_folder, Some("/home/input".into()));
    }

    #[test]
    fn text_scale_roundtrips_and_seeds_clamped() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg_path = dir.path().join("test_config.json");

        let original = ConverterConfig {
            last_input_folder: None,
            last_offload_parent: None,
            text_scale_percent: Some(150),
        };
        save_to(&cfg_path, &original);
        assert_eq!(load_from(&cfg_path).text_scale_percent, Some(150));

        // Pre-feature config files (field absent) deserialize to None.
        std::fs::write(
            &cfg_path,
            r#"{"last_input_folder": null, "last_offload_parent": null}"#,
        )
        .unwrap();
        assert_eq!(load_from(&cfg_path).text_scale_percent, None);

        let mut snapshot = crate::state::AppStateSnapshot::initial();
        snapshot.text_scale_percent = 100;
        seed_snapshot_from_config(
            &mut snapshot,
            &ConverterConfig {
                last_input_folder: None,
                last_offload_parent: None,
                text_scale_percent: Some(10_000),
            },
        );
        assert_eq!(
            snapshot.text_scale_percent, 400,
            "out-of-range persisted values clamp on seeding",
        );
    }

    #[test]
    fn seed_snapshot_from_config_restores_offload_parent_if_exists() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(dir.path().exists());

        let cfg = ConverterConfig {
            last_input_folder: None,
            last_offload_parent: Some(dir.path().to_string_lossy().to_string()),
            text_scale_percent: None,
        };

        let mut snapshot = crate::state::AppStateSnapshot::initial();
        seed_snapshot_from_config(&mut snapshot, &cfg);

        assert_eq!(
            snapshot.offload.parent_folder,
            Some(dir.path().to_path_buf()),
            "existing offload parent should be restored",
        );
    }

    #[test]
    fn seed_snapshot_from_config_ignores_missing_offload_parent() {
        let cfg = ConverterConfig {
            last_input_folder: None,
            last_offload_parent: Some("/nonexistent/path/that/does/not/exist_42".into()),
            text_scale_percent: None,
        };

        let mut snapshot = crate::state::AppStateSnapshot::initial();
        seed_snapshot_from_config(&mut snapshot, &cfg);

        assert_eq!(
            snapshot.offload.parent_folder, None,
            "non-existent offload parent should be ignored",
        );
    }

    #[test]
    fn seed_snapshot_from_config_does_not_seed_output_folder() {
        let cfg = ConverterConfig {
            last_input_folder: None,
            last_offload_parent: None,
            text_scale_percent: None,
        };

        let mut snapshot = crate::state::AppStateSnapshot::initial();
        // Seed from config (no output folder in config anymore)
        seed_snapshot_from_config(&mut snapshot, &cfg);

        // Output folder should remain at its initial default (empty)
        assert!(
            snapshot.converter.settings.output_folder.as_os_str().is_empty(),
            "output folder must not be seeded by config anymore; it defaults to source clip parent dir",
        );
    }

    // Env-var mutation is serialized: config_base_dir reads the process env,
    // and sibling lib tests could otherwise race the override.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn config_base_dir_honors_ltc_config_home_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        // SAFETY: single-threaded w.r.t. env (ENV_LOCK held); restoring
        // afterwards keeps sibling tests on the default resolution.
        // (set_var/remove_var are not yet safe in std, so unwrap the Result
        // is avoided on platforms where they are fns.)
        std::env::set_var("LTC_CONFIG_HOME", dir.path());
        let base = config_base_dir().expect("override must always resolve");
        assert_eq!(base, dir.path());
        // config_path() resolves through config_base_dir(), so with the
        // override set the converter config round-trips inside the
        // override dir and never touches the real user config.
        save(&ConverterConfig {
            last_input_folder: Some("/does/not/matter".into()),
            last_offload_parent: None,
            text_scale_percent: None,
        });
        let expected = dir.path().join(CONFIG_DIR).join(CONFIG_FILE);
        assert!(
            expected.exists(),
            "config must be written under the override dir"
        );
        assert_eq!(
            load().last_input_folder,
            Some("/does/not/matter".to_string())
        );
        std::env::remove_var("LTC_CONFIG_HOME");
    }

    #[test]
    fn config_base_dir_falls_through_when_override_unset_or_empty() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("LTC_CONFIG_HOME");
        let plain = config_base_dir();
        std::env::set_var("LTC_CONFIG_HOME", "");
        let empty = config_base_dir();
        // Empty override is treated as unset — both fall through to
        // dirs::config_dir() (same Some/None shape, same value).
        assert_eq!(plain.is_some(), empty.is_some());
        assert_eq!(plain, empty);
        std::env::remove_var("LTC_CONFIG_HOME");
    }

    // ── Session store ────────────────────────────────────────────────────

    fn sample_session(id: &str, name: &str) -> SessionRecord {
        SessionRecord::new(
            id.to_string(),
            name.to_string(),
            "2026-11-05".to_string(),
            25.0,
            false,
            Timecode {
                hours: 1,
                minutes: 0,
                seconds: 0,
                frames: 0,
            },
        )
    }

    #[test]
    fn session_record_roundtrips_through_json() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut rec = sample_session("2026-11-05-forest-shoot", "Forest Shoot");
        rec.fps = 29.97;
        rec.drop_frame = true;
        rec.last_emitted_timecode = Timecode {
            hours: 1,
            minutes: 47,
            seconds: 13,
            frames: 20,
        };
        rec.continue_gap_seconds = 90;
        rec.offload_destinations = vec!["/media/backup/2026-11-05-forest-shoot".to_string()];
        rec.last_active_date = "2026-11-06".to_string();

        save_session_to(dir.path(), &rec).expect("save must succeed");

        let loaded = load_sessions_from(dir.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0], rec);
    }

    #[test]
    fn session_load_skips_corrupt_files_with_all_others_intact() {
        let dir = tempfile::TempDir::new().unwrap();
        let good = sample_session("2026-11-05-good", "Good");
        save_session_to(dir.path(), &good).unwrap();
        std::fs::write(dir.path().join("2026-11-04-broken.json"), "{ not json").unwrap();
        // A non-json file (e.g. leftover temp file) must be ignored entirely.
        std::fs::write(dir.path().join("stray.txt"), "junk").unwrap();

        let loaded = load_sessions_from(dir.path());
        assert_eq!(
            loaded.len(),
            1,
            "corrupt session files must be skipped, not fatal"
        );
        assert_eq!(loaded[0].id, good.id);
    }

    #[test]
    fn session_load_ignores_unknown_fields_for_forward_compat() {
        let dir = tempfile::TempDir::new().unwrap();
        let rec = sample_session("2026-11-05-fwd", "Fwd");
        save_session_to(dir.path(), &rec).unwrap();
        // Inject a field a future schema might add; current load must keep it.
        let path = dir.path().join("2026-11-05-fwd.json");
        let content = std::fs::read_to_string(&path).unwrap();
        let patched = content.replace(
            &format!("\"schema\": \"{}\"", SESSION_SCHEMA),
            &format!("\"schema\": \"{}\", \"future_field\": 42", SESSION_SCHEMA),
        );
        std::fs::write(&path, patched).unwrap();

        let loaded = load_sessions_from(dir.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0], rec);
    }

    #[test]
    fn session_delete_removes_file_and_reports_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        let rec = sample_session("2026-11-05-gone", "Gone");
        save_session_to(dir.path(), &rec).unwrap();

        assert!(delete_session_from(dir.path(), &rec.id).unwrap());
        assert!(!delete_session_from(dir.path(), &rec.id).unwrap());
        assert!(load_sessions_from(dir.path()).is_empty());
        // Traversal-shaped ids never delete anything.
        assert!(!delete_session_from(dir.path(), "../escape").unwrap());
    }

    #[test]
    fn session_save_overwrites_existing_file_atomically() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut rec = sample_session("2026-11-05-twice", "Twice");
        save_session_to(dir.path(), &rec).unwrap();
        rec.last_emitted_timecode = Timecode {
            hours: 2,
            minutes: 0,
            seconds: 0,
            frames: 0,
        };
        save_session_to(dir.path(), &rec).unwrap();

        let loaded = load_sessions_from(dir.path());
        assert_eq!(loaded.len(), 1, "re-saving the same id must overwrite");
        assert_eq!(loaded[0].last_emitted_timecode, rec.last_emitted_timecode);
        // No temp leftovers.
        assert!(!dir.path().join(format!("{}.json.tmp", rec.id)).exists());
    }

    #[test]
    fn slugify_produces_kebab_case() {
        assert_eq!(slugify_session_name("Forest Shoot"), "forest-shoot");
        assert_eq!(
            slugify_session_name("  Multi   Space / Weird! "),
            "multi-space-weird"
        );
        // Non-ASCII letters are separators, not slug characters.
        assert_eq!(slugify_session_name("\u{00dc}n\u{ef}code"), "n-code");
        assert_eq!(slugify_session_name("..."), "session");
        assert_eq!(slugify_session_name(""), "session");
        assert_eq!(slugify_session_name("ABC-123"), "abc-123");
    }

    #[test]
    fn make_session_id_collides_with_numbered_suffix() {
        let existing = vec!["2026-11-05-shoot".to_string()];
        assert_eq!(
            make_session_id("Shoot", "2026-11-05", &existing),
            "2026-11-05-shoot-2"
        );
        let existing2 = vec![
            "2026-11-05-shoot".to_string(),
            "2026-11-05-shoot-2".to_string(),
        ];
        assert_eq!(
            make_session_id("Shoot", "2026-11-05", &existing2),
            "2026-11-05-shoot-3"
        );
        let none: Vec<String> = Vec::new();
        assert_eq!(
            make_session_id("Shoot", "2026-11-05", &none),
            "2026-11-05-shoot"
        );
    }

    #[test]
    fn session_continue_timecode_uses_gap_and_fps() {
        let mut rec = sample_session("2026-11-05-c", "C");
        rec.last_emitted_timecode = Timecode {
            hours: 1,
            minutes: 47,
            seconds: 13,
            frames: 20,
        };
        assert_eq!(
            rec.continue_timecode(),
            Timecode {
                hours: 1,
                minutes: 48,
                seconds: 13,
                frames: 20
            },
            "default gap 60 s at 25 fps = +1500 frames"
        );
        rec.continue_gap_seconds = 0;
        assert_eq!(rec.continue_timecode(), rec.last_emitted_timecode);
    }

    #[test]
    fn sessions_dir_resolves_under_config_base() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        std::env::set_var("LTC_CONFIG_HOME", dir.path());
        let expected = dir.path().join(CONFIG_DIR).join(SESSIONS_DIR);
        assert_eq!(sessions_dir(), Some(expected));
        std::env::remove_var("LTC_CONFIG_HOME");
    }
}
