use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

const CONFIG_DIR: &str = "ltc-timecode-generator";
const CONFIG_FILE: &str = "converter_config.json";

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct ConverterConfig {
    pub last_input_folder: Option<String>,
    pub last_output_folder: Option<String>,
}

fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|base| base.join(CONFIG_DIR).join(CONFIG_FILE))
}

pub fn load() -> ConverterConfig {
    let path = match config_path() {
        Some(p) => p,
        None => return ConverterConfig::default(),
    };
    match fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => ConverterConfig::default(),
    }
}

pub fn save(config: &ConverterConfig) {
    let path = match config_path() {
        Some(p) => p,
        None => return,
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(content) = serde_json::to_string_pretty(config) {
        let _ = fs::write(&path, content);
    }
}

pub fn save_input_folder(path: &std::path::Path) {
    let mut cfg = load();
    cfg.last_input_folder = Some(path.to_string_lossy().to_string());
    save(&cfg);
}

pub fn save_output_folder(path: &std::path::Path) {
    let mut cfg = load();
    cfg.last_output_folder = Some(path.to_string_lossy().to_string());
    save(&cfg);
}