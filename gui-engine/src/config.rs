use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) const CONFIG_DIR: &str = "ltc-timecode-generator";
const CONFIG_FILE: &str = "converter_config.json";

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
}
