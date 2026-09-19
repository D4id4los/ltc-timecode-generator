use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regex::Regex;

#[derive(Clone, Debug)]
pub struct FileNamingPattern {
    pub name: &'static str,
    pub description: &'static str,
    pub regex: &'static str,
    pub channel_group_index: usize,
}

pub static BUILTIN_PATTERNS: &[FileNamingPattern] = &[FileNamingPattern {
    name: "TASCAM",
    description: "Tascam Portacapture X8 — name prefix + S<channel>",
    regex: r"^(.+?)S(\d+)$",
    channel_group_index: 2,
}];

pub fn match_files_to_groups(
    folder: &Path,
    pattern: &FileNamingPattern,
) -> BTreeMap<String, Vec<PathBuf>> {
    let mut groups: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();

    let re = match Regex::new(pattern.regex) {
        Ok(r) => r,
        Err(e) => {
            log::error!("Invalid regex '{}': {}", pattern.regex, e);
            return groups;
        }
    };

    let dir = match folder.read_dir() {
        Ok(d) => d,
        Err(e) => {
            log::error!("Cannot read directory {:?}: {}", folder, e);
            return groups;
        }
    };

    for entry in dir.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };

        if let Some(caps) = re.captures(&stem) {
            let prefix = caps
                .get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let _channel: u32 = caps
                .get(pattern.channel_group_index)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);

            groups
                .entry(prefix)
                .or_default()
                .push(path);
        }
    }

    for files in groups.values_mut() {
        files.sort_by(|a, b| {
            let a_stem = a.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let b_stem = b.file_stem().and_then(|s| s.to_str()).unwrap_or("");

            let a_ch: u32 = re
                .captures(a_stem)
                .and_then(|c| c.get(pattern.channel_group_index))
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(u32::MAX);
            let b_ch: u32 = re
                .captures(b_stem)
                .and_then(|c| c.get(pattern.channel_group_index))
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(u32::MAX);

            a_ch.cmp(&b_ch)
        });
    }

    groups
}

pub fn default_output_filename(group_name: &str, container: &str) -> String {
    format!("{}-multi-audio-vid.{}", group_name, container)
}