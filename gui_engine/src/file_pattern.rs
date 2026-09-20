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

pub static BUILTIN_PATTERNS: &[FileNamingPattern] = &[
    FileNamingPattern {
        name: "TASCAM",
        description: "Tascam Portacapture X8 — name prefix + S<channel>",
        regex: r"^(.+?)S(\d+)$",
        channel_group_index: 2,
    },
    FileNamingPattern {
        name: "* (any)",
        description: "Any audio file — select files directly",
        regex: r"^.*$",
        channel_group_index: 2,
    },
];

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

pub fn wrap_user_selected_files(files: Vec<PathBuf>) -> BTreeMap<String, Vec<PathBuf>> {
    let mut groups: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    if files.is_empty() {
        return groups;
    }

    let mut sorted = files;
    sorted.sort();

    let prefix = sorted[0]
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("selected")
        .to_string();

    groups.insert(prefix, sorted);
    groups
}

pub fn default_output_filename(group_name: &str, container: &str) -> String {
    format!("{}-multi-audio-vid.{}", group_name, container)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::path::Path;

    #[test]
    fn test_default_output_filename_mkv() {
        assert_eq!(
            default_output_filename("myrecording", "mkv"),
            "myrecording-multi-audio-vid.mkv"
        );
    }

    #[test]
    fn test_default_output_filename_mov() {
        assert_eq!(
            default_output_filename("test123", "mov"),
            "test123-multi-audio-vid.mov"
        );
    }

    #[test]
    fn test_default_output_filename_mp4() {
        assert_eq!(
            default_output_filename("clip_A", "mp4"),
            "clip_A-multi-audio-vid.mp4"
        );
    }

    #[test]
    fn test_default_output_filename_empty_group() {
        assert_eq!(
            default_output_filename("", "mkv"),
            "-multi-audio-vid.mkv"
        );
    }

    #[test]
    fn test_default_output_filename_unknown_container() {
        assert_eq!(
            default_output_filename("rec", "webm"),
            "rec-multi-audio-vid.webm"
        );
    }

    // ── wrap_user_selected_files ──────────────────────────────────────────

    #[test]
    fn test_wrap_empty_files() {
        let result = wrap_user_selected_files(vec![]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_wrap_single_file() {
        let files = vec![PathBuf::from("/tmp/recording_S01.wav")];
        let result = wrap_user_selected_files(files);
        assert_eq!(result.len(), 1);
        let key = result.keys().next().unwrap();
        assert_eq!(key, "recording_S01");
        let vals = result.values().next().unwrap();
        assert_eq!(vals.len(), 1);
    }

    #[test]
    fn test_wrap_multiple_files_sorted() {
        let files = vec![
            PathBuf::from("/tmp/ch3.wav"),
            PathBuf::from("/tmp/ch1.wav"),
            PathBuf::from("/tmp/ch2.wav"),
        ];
        let result = wrap_user_selected_files(files);
        assert_eq!(result.len(), 1);
        let vals = result.values().next().unwrap();
        assert_eq!(vals.len(), 3);
        assert_eq!(vals[0].file_stem().unwrap(), "ch1");
        assert_eq!(vals[1].file_stem().unwrap(), "ch2");
        assert_eq!(vals[2].file_stem().unwrap(), "ch3");
    }

    #[test]
    fn test_wrap_same_prefix_used() {
        let files = vec![
            PathBuf::from("/data/take1_A.wav"),
            PathBuf::from("/data/take1_B.wav"),
        ];
        let result = wrap_user_selected_files(files);
        assert_eq!(result.len(), 1);
        assert!(result.contains_key("take1_A"));
    }

    // ── match_files_to_groups (needs temp dir) ────────────────────────────

    #[test]
    fn test_match_files_empty_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        let result = match_files_to_groups(dir.path(), &BUILTIN_PATTERNS[0]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_match_files_tascam_pattern() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();
        std::fs::write(base.join("take1_S01.wav"), b"data").unwrap();
        std::fs::write(base.join("take1_S02.wav"), b"data").unwrap();
        std::fs::write(base.join("take1_S03.wav"), b"data").unwrap();
        std::fs::write(base.join("unrelated.txt"), b"text").unwrap();

        let result = match_files_to_groups(base, &BUILTIN_PATTERNS[0]);
        assert_eq!(result.len(), 1, "expected 1 group, got {:?}", result.keys());
        let group = result.get("take1_").unwrap();
        assert_eq!(group.len(), 3, "expected 3 files in group");
        assert!(group[0].to_string_lossy().ends_with("take1_S01.wav"));
    }

    #[test]
    fn test_match_files_tascam_multiple_prefixes() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();
        std::fs::write(base.join("scene1_S01.wav"), b"data").unwrap();
        std::fs::write(base.join("scene1_S02.wav"), b"data").unwrap();
        std::fs::write(base.join("scene2_S01.wav"), b"data").unwrap();
        std::fs::write(base.join("scene2_S02.wav"), b"data").unwrap();

        let result = match_files_to_groups(base, &BUILTIN_PATTERNS[0]);
        assert_eq!(result.len(), 2);
        assert!(result.contains_key("scene1_"));
        assert!(result.contains_key("scene2_"));
        assert_eq!(result.get("scene1_").unwrap().len(), 2);
        assert_eq!(result.get("scene2_").unwrap().len(), 2);
    }

    #[test]
    fn test_match_files_any_pattern() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();
        std::fs::write(base.join("anything.wav"), b"data").unwrap();
        std::fs::write(base.join("foo.bar"), b"data").unwrap();
        std::fs::write(base.join("no_ext"), b"data").unwrap();

        let result = match_files_to_groups(base, &BUILTIN_PATTERNS[1]);
        assert_eq!(result.len(), 1);
        assert_eq!(result.values().next().unwrap().len(), 3);
    }

    #[test]
    fn test_match_files_sorted_by_channel() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();
        std::fs::write(base.join("take_S03.wav"), b"data").unwrap();
        std::fs::write(base.join("take_S01.wav"), b"data").unwrap();
        std::fs::write(base.join("take_S02.wav"), b"data").unwrap();

        let result = match_files_to_groups(base, &BUILTIN_PATTERNS[0]);
        let files = result.get("take_").unwrap();
        assert_eq!(files.len(), 3);
        assert!(files[0].to_string_lossy().ends_with("take_S01.wav"));
        assert!(files[1].to_string_lossy().ends_with("take_S02.wav"));
        assert!(files[2].to_string_lossy().ends_with("take_S03.wav"));
    }

    #[test]
    fn test_match_files_bad_pattern_regex() {
        let bad_pattern = FileNamingPattern {
            name: "bad",
            description: "broken regex",
            regex: r"[invalid",
            channel_group_index: 1,
        };
        let dir = tempfile::TempDir::new().unwrap();
        let result = match_files_to_groups(dir.path(), &bad_pattern);
        assert!(result.is_empty());
    }

    #[test]
    fn test_match_files_nonexistent_directory() {
        let result = match_files_to_groups(
            Path::new("/nonexistent_dir_abc123"),
            &BUILTIN_PATTERNS[0],
        );
        assert!(result.is_empty());
    }
}