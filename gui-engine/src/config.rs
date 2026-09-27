use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const CONFIG_DIR: &str = "ltc-timecode-generator";
const CONFIG_FILE: &str = "converter_config.json";

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct ConverterConfig {
    pub last_input_folder: Option<String>,
    pub last_output_folder: Option<String>,
    pub last_offload_parent: Option<String>,
}

fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|base| base.join(CONFIG_DIR).join(CONFIG_FILE))
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

pub fn save_output_folder(path: &Path) {
    let mut cfg = load();
    cfg.last_output_folder = Some(path.to_string_lossy().to_string());
    save(&cfg);
}

pub fn save_offload_parent(path: &Path) {
    let mut cfg = load();
    cfg.last_offload_parent = Some(path.to_string_lossy().to_string());
    save(&cfg);
}

/// Seed engine snapshot fields from a loaded config.
/// Returns `true` if any field was populated (for diagnostics).
pub fn seed_snapshot_from_config(
    snapshot: &mut crate::state::AppStateSnapshot,
    cfg: &ConverterConfig,
) {
    // Output folder (converter)
    if let Some(ref folder) = cfg.last_output_folder {
        snapshot.converter.settings.output_folder = Path::new(folder).to_path_buf();
    }
    // Offload parent folder
    if let Some(ref folder) = cfg.last_offload_parent {
        let p = Path::new(folder);
        if p.exists() {
            snapshot.offload.parent_folder = Some(p.to_path_buf());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_load_offload_parent_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg_path = dir.path().join("test_config.json");

        let original = ConverterConfig {
            last_input_folder: Some("/home/input".into()),
            last_output_folder: Some("/home/output".into()),
            last_offload_parent: None,
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
        assert_eq!(reloaded.last_output_folder, Some("/home/output".into()));
    }

    #[test]
    fn seed_snapshot_from_config_restores_offload_parent_if_exists() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(dir.path().exists());

        let cfg = ConverterConfig {
            last_input_folder: None,
            last_output_folder: None,
            last_offload_parent: Some(dir.path().to_string_lossy().to_string()),
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
            last_output_folder: None,
            last_offload_parent: Some("/nonexistent/path/that/does/not/exist_42".into()),
        };

        let mut snapshot = crate::state::AppStateSnapshot::initial();
        seed_snapshot_from_config(&mut snapshot, &cfg);

        assert_eq!(
            snapshot.offload.parent_folder,
            None,
            "non-existent offload parent should be ignored",
        );
    }

    #[test]
    fn seed_snapshot_from_config_restores_output_folder() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = ConverterConfig {
            last_input_folder: None,
            last_output_folder: Some(dir.path().to_string_lossy().to_string()),
            last_offload_parent: None,
        };

        let mut snapshot = crate::state::AppStateSnapshot::initial();
        seed_snapshot_from_config(&mut snapshot, &cfg);

        assert_eq!(
            snapshot.converter.settings.output_folder,
            dir.path(),
        );
    }
}