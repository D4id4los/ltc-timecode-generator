use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use regex::Regex;

#[derive(Clone, Debug)]
pub struct FileNamingPattern {
    pub name: &'static str,
    pub description: &'static str,
    pub regex: &'static str,
    pub prefix_group: &'static str,
    pub channel_group: Option<&'static str>,
}

pub static BUILTIN_PATTERNS: &[FileNamingPattern] = &[
    FileNamingPattern {
        name: "TASCAM",
        description: "Tascam Portacapture X8 — name prefix + S<channel>",
        regex: r"^(?P<prefix>.+?)S(?P<channel>\d+)$",
        prefix_group: "prefix",
        channel_group: Some("channel"),
    },
    FileNamingPattern {
        name: "* (any)",
        description: "Any file — select files directly",
        regex: r"^.*$",
        prefix_group: "prefix",
        channel_group: None,
    },
];

/// Camera-specific naming patterns (used alongside TASCAM, not `* (any)`).
pub static CAMERA_PATTERNS: &[FileNamingPattern] = &[
    FileNamingPattern {
        name: "Sony Handycam",
        description: "Sony Handycam — Cxxxx.MP4/MTS",
        regex: r"^(?P<prefix>.*C\d{4}.*)\.(?:mp4|MP4|MTS|mts|M4V|m4v|)$",
        prefix_group: "prefix",
        channel_group: None,
    },
    FileNamingPattern {
        name: "Sony FS100",
        description: "Sony FS100 — xxxxx.MTS",
        regex: r"^(?P<prefix>.*\d{5}.*)\.(?:MTS|mts|M4V|m4v)$",
        prefix_group: "prefix",
        channel_group: None,
    },
    FileNamingPattern {
        name: "Canon",
        description: "Canon — MVI_xxxx.MP4",
        regex: r"^(?P<prefix>.*MVI_\d{4}.*)\.(?:mp4|MP4)$",
        prefix_group: "prefix",
        channel_group: None,
    },
    FileNamingPattern {
        name: "Panasonic",
        description: "Panasonic — GHxxxxx.MP4",
        regex: r"^(?P<prefix>.*GH\d{5}.*)\.(?:mp4|MP4)$",
        prefix_group: "prefix",
        channel_group: None,
    },
    FileNamingPattern {
        name: "GoPro",
        description: "GoPro — GOPRxxxx/GPxxxxxx.MP4",
        regex: r"^(?P<prefix>.*(?:GOPR\d{4}|GP\d{6}).*)\.(?:mp4|MP4)$",
        prefix_group: "prefix",
        channel_group: None,
    },
];

/// Directories to skip even when not hidden (dot-prefixed hidden dirs are
/// skipped unconditionally).
const JUNK_DIRS: &[&str] = &[
    "__MACOSX",
    "System Volume Information",
    "$RECYCLE.BIN",
    "LOST.DIR",
];

const MAX_SCAN_DEPTH: usize = 8;
const MAX_SCAN_FILES: usize = 20_000;

/// Recursively collect all regular files under `root`, skipping hidden entries,
/// well-known junk directories, and symlinks (to avoid loops).
/// Files are returned in a depth-first order, sorted lexicographically within
/// each directory for deterministic output.
fn collect_files_recursive(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = Vec::new();
    stack.push((root.to_path_buf(), 0));

    while let Some((dir, depth)) = stack.pop() {
        if depth > MAX_SCAN_DEPTH {
            continue;
        }

        let read_dir = match dir.read_dir() {
            Ok(d) => d,
            Err(e) => {
                log::warn!("Cannot read directory {:?}: {}", dir, e);
                continue;
            }
        };

        let mut entries: Vec<_> = read_dir.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());

        for entry in &entries {
            if files.len() >= MAX_SCAN_FILES {
                break;
            }

            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            // Skip hidden entries (dot-prefixed)
            if name_str.starts_with('.') {
                continue;
            }

            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };

            // Skip symlinks entirely (no follow)
            if ft.is_symlink() {
                continue;
            }

            if ft.is_dir() {
                if JUNK_DIRS.contains(&name_str.as_ref()) {
                    continue;
                }
                stack.push((entry.path(), depth + 1));
            } else if ft.is_file() {
                files.push(entry.path());
            }
        }

        if files.len() >= MAX_SCAN_FILES {
            break;
        }
    }

    files
}

/// Compute the relative parent directory of `path` under `root`.
/// Root-level files return an empty string. Subdirectories are separated with
/// forward slashes.
fn relative_dir(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let parent = rel.parent().and_then(|p| p.to_str()).unwrap_or("");
    parent.replace('\\', "/")
}

/// Build a display group key from a raw prefix and a relative directory.
/// For root-level files (empty `rel_dir`) this is the plain prefix.
/// For nested files the format is `prefix (rel/dir)`.
pub fn group_display_key(prefix: &str, rel_dir: &str) -> String {
    if rel_dir.is_empty() {
        prefix.to_string()
    } else {
        format!("{} ({})", prefix, rel_dir)
    }
}

/// Extract the raw prefix from a display key produced by [`group_display_key`].
/// If the key ends with ` (something)`, the parenthesised suffix is stripped.
/// Otherwise the whole key is returned unchanged.
pub fn group_key_prefix(key: &str) -> &str {
    if let Some(start) = key.rfind(" (") {
        let after = &key[start + 2..];
        if after.ends_with(')') {
            return &key[..start];
        }
    }
    key
}

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

    let files = collect_files_recursive(folder);

    // Use (rel_dir, prefix) as internal group key to keep files from different
    // subdirectories separate even when their raw prefix is identical.
    let mut grouped: BTreeMap<(String, String), Vec<PathBuf>> = BTreeMap::new();

    for path in &files {
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };

        if let Some(caps) = re.captures(&stem) {
            let prefix = caps
                .name(pattern.prefix_group)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let rel = relative_dir(folder, path);
            grouped.entry((rel, prefix)).or_default().push(path.clone());
        }
    }

    for ((rel_dir, prefix), mut files) in grouped {
        files.sort_by(|a, b| {
            let a_stem = a.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let b_stem = b.file_stem().and_then(|s| s.to_str()).unwrap_or("");

            let a_ch: u32 = {
                let ch_name = pattern.channel_group;
                re.captures(a_stem)
                    .and_then(|c| ch_name.and_then(move |n| c.name(n)))
                    .and_then(|m| m.as_str().parse().ok())
                    .unwrap_or(u32::MAX)
            };
            let b_ch: u32 = {
                let ch_name = pattern.channel_group;
                re.captures(b_stem)
                    .and_then(|c| ch_name.and_then(move |n| c.name(n)))
                    .and_then(|m| m.as_str().parse().ok())
                    .unwrap_or(u32::MAX)
            };

            a_ch.cmp(&b_ch)
        });

        let key = group_display_key(&prefix, &rel_dir);
        groups.insert(key, files);
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

/// Matched group from scanning a folder with all applicable patterns.
#[derive(Clone, Debug)]
pub struct MatchedGroup {
    pub prefix: String,
    /// Relative directory under the scanned root (empty for root-level files).
    pub rel_dir: String,
    pub files: Vec<PathBuf>,
    pub pattern_name: &'static str,
    pub recording_type: crate::converter::RecordingType,
}

/// Scan a folder with all built-in patterns (TASCAM + camera) simultaneously.
/// Files matching multiple patterns are assigned to the first match (TASCAM first).
/// Returns groups tagged with their `RecordingType`.
pub fn match_files_all_patterns(folder: &Path) -> Vec<MatchedGroup> {
    let mut used_paths: HashSet<PathBuf> = HashSet::new();
    let mut results: Vec<MatchedGroup> = Vec::new();

    // TASCAM pattern (audio) — limited to .wav only
    let audio_patterns = &BUILTIN_PATTERNS[..1];
    let all_patterns: Vec<&FileNamingPattern> =
        audio_patterns.iter().chain(CAMERA_PATTERNS.iter()).collect();

    let files = collect_files_recursive(folder);

    for pattern in &all_patterns {
        let re = match Regex::new(pattern.regex) {
            Ok(r) => r,
            Err(_) => continue,
        };

        // Group by (rel_dir, prefix) so files with the same raw prefix but
        // located in different subdirectories stay separate.
        let mut group_map: BTreeMap<(String, String), Vec<PathBuf>> = BTreeMap::new();

        for path in &files {
            if used_paths.contains(path) {
                continue;
            }

            let file_name = match path.file_name().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };

            // Track which form to use for matching (stems for TASCAM, full
            // name for cameras)
            let match_str = if pattern.name == "TASCAM" {
                &stem
            } else {
                &file_name
            };

            if let Some(caps) = re.captures(match_str) {
                let prefix = caps
                    .name(pattern.prefix_group)
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default();
                let rel = relative_dir(folder, path);
                used_paths.insert(path.clone());
                group_map.entry((rel, prefix)).or_default().push(path.clone());
            }
        }

        // Sort files in each group by the capture index (for TASCAM) or by
        // sequential number (for cameras)
        for ((rel_dir, prefix), mut files) in group_map {
            files.sort_by(|a, b| {
                let a_str = if pattern.name == "TASCAM" {
                    a.file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string()
                } else {
                    a.file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string()
                };
                let b_str = if pattern.name == "TASCAM" {
                    b.file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string()
                } else {
                    b.file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string()
                };
                let a_ch: u32 = {
                    let ch_name = pattern.channel_group;
                    re.captures(&a_str)
                        .and_then(|c| ch_name.and_then(move |n| c.name(n)))
                        .and_then(|m| m.as_str().parse().ok())
                        .unwrap_or(u32::MAX)
                };
                let b_ch: u32 = {
                    let ch_name = pattern.channel_group;
                    re.captures(&b_str)
                        .and_then(|c| ch_name.and_then(move |n| c.name(n)))
                        .and_then(|m| m.as_str().parse().ok())
                        .unwrap_or(u32::MAX)
                };
                a_ch.cmp(&b_ch)
            });

            let recording_type = if pattern.name == "TASCAM" {
                crate::converter::RecordingType::MultiTrackAudio
            } else {
                crate::converter::RecordingType::VideoClipSequence
            };

            results.push(MatchedGroup {
                prefix,
                rel_dir,
                files,
                pattern_name: pattern.name,
                recording_type,
            });
        }
    }

    // Consecutive-numbering heuristic for video clips:
    // Group consecutive numbered files within the same camera pattern AND the
    // same relative directory.
    let mut merged = Vec::new();
    let mut i = 0;
    while i < results.len() {
        let mut group = results[i].clone();
        if group.recording_type == crate::converter::RecordingType::VideoClipSequence {
            let base_prefix = group
                .prefix
                .trim_end_matches(|c: char| c.is_ascii_digit())
                .to_string();
            let mut j = i + 1;
            while j < results.len() {
                let next = &results[j];
                if next.recording_type != crate::converter::RecordingType::VideoClipSequence {
                    break;
                }
                // Only merge groups within the same subdirectory
                if next.rel_dir != group.rel_dir {
                    break;
                }
                let next_base = next
                    .prefix
                    .trim_end_matches(|c: char| c.is_ascii_digit())
                    .to_string();
                if next_base != base_prefix {
                    break;
                }
                group.files.extend(next.files.clone());
                j += 1;
            }
            i = j;
        } else {
            i += 1;
        }
        merged.push(group);
    }

    merged
}

pub fn default_output_filename(group_name: &str, container: &str) -> String {
    format!("{}-multi-audio-vid.{}", group_name, container)
}

#[cfg(test)]
mod tests {
    use super::*;
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

    // ── group_display_key / group_key_prefix ──────────────────────────────

    #[test]
    fn test_group_display_key_root_level() {
        assert_eq!(group_display_key("take1_", ""), "take1_");
        assert_eq!(group_display_key("C0001", ""), "C0001");
    }

    #[test]
    fn test_group_display_key_nested() {
        assert_eq!(
            group_display_key("take1_", "subdir"),
            "take1_ (subdir)"
        );
        assert_eq!(
            group_display_key("C0001", "day1/card2"),
            "C0001 (day1/card2)"
        );
    }

    #[test]
    fn test_group_key_prefix_root_level() {
        assert_eq!(group_key_prefix("take1_"), "take1_");
        assert_eq!(group_key_prefix("C0001"), "C0001");
    }

    #[test]
    fn test_group_key_prefix_nested() {
        assert_eq!(group_key_prefix("take1_ (subdir)"), "take1_");
        assert_eq!(
            group_key_prefix("C0001 (day1/card2)"),
            "C0001"
        );
    }

    #[test]
    fn test_group_key_prefix_no_false_positive() {
        // A key ending without a closing paren after the last " (" should
        // be treated as the whole string.
        assert_eq!(group_key_prefix("take1_ (no end"), "take1_ (no end");
        assert_eq!(group_key_prefix("plain"), "plain");
        assert_eq!(group_key_prefix("foo (bar) baz"), "foo (bar) baz");
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
            prefix_group: "prefix",
            channel_group: Some("channel"),
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

    // ── Recursive match_files_to_groups tests ─────────────────────────────

    #[test]
    fn test_recursive_nested_tascam() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();
        // Root-level files
        std::fs::write(base.join("take1_S01.wav"), b"data").unwrap();
        std::fs::write(base.join("take1_S02.wav"), b"data").unwrap();
        // Nested files
        let sub = base.join("card2");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("take1_S01.wav"), b"data").unwrap();
        std::fs::write(sub.join("take1_S02.wav"), b"data").unwrap();

        let result = match_files_to_groups(base, &BUILTIN_PATTERNS[0]);

        // Two groups: root-level "take1_" and nested "take1_ (card2)"
        assert_eq!(result.len(), 2, "expected 2 separate groups, got {:?}", result.keys());
        let root_group = result.get("take1_").expect("missing root group");
        assert_eq!(root_group.len(), 2);
        assert!(root_group[0].to_string_lossy().ends_with("take1_S01.wav"));

        let nested_key = "take1_ (card2)";
        let nested_group = result.get(nested_key).expect("missing nested group");
        assert_eq!(nested_group.len(), 2);
    }

    #[test]
    fn test_recursive_same_prefix_across_deep_dirs() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();
        let sub1 = base.join("day1");
        let sub2 = base.join("day2");
        std::fs::create_dir(&sub1).unwrap();
        std::fs::create_dir(&sub2).unwrap();
        std::fs::write(sub1.join("scene_S01.wav"), b"data").unwrap();
        std::fs::write(sub2.join("scene_S01.wav"), b"data").unwrap();

        let result = match_files_to_groups(base, &BUILTIN_PATTERNS[0]);
        assert_eq!(result.len(), 2, "expected two separate groups");
        assert!(result.contains_key("scene_ (day1)"));
        assert!(result.contains_key("scene_ (day2)"));
    }

    #[test]
    fn test_recursive_flat_dir_backward_compat() {
        // A flat directory (no subdirs) must produce the same keys as before.
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();
        std::fs::write(base.join("take1_S01.wav"), b"data").unwrap();
        std::fs::write(base.join("take1_S02.wav"), b"data").unwrap();
        std::fs::write(base.join("scene1_S01.wav"), b"data").unwrap();

        let result = match_files_to_groups(base, &BUILTIN_PATTERNS[0]);
        assert_eq!(result.len(), 2);
        assert!(result.contains_key("take1_"), "expected plain key 'take1_'");
        assert!(result.contains_key("scene1_"), "expected plain key 'scene1_'");
    }

    #[test]
    fn test_recursive_skips_hidden_dirs() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();
        let hidden = base.join(".hidden");
        std::fs::create_dir(&hidden).unwrap();
        std::fs::write(hidden.join("take1_S01.wav"), b"data").unwrap();

        let result = match_files_to_groups(base, &BUILTIN_PATTERNS[0]);
        assert!(result.is_empty(), "files in hidden dirs must be skipped");
    }

    #[test]
    fn test_recursive_skips_junk_dirs() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();
        let junk = base.join("__MACOSX");
        std::fs::create_dir(&junk).unwrap();
        std::fs::write(junk.join("take1_S01.wav"), b"data").unwrap();

        let result = match_files_to_groups(base, &BUILTIN_PATTERNS[0]);
        assert!(result.is_empty(), "files in junk dirs must be skipped");
    }

    #[test]
    fn test_recursive_skips_symlinked_dirs() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();
        let real = base.join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("take1_S01.wav"), b"data").unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let result = match_files_to_groups(base, &BUILTIN_PATTERNS[0]);
        // The file in `real/` is found through the recursive scan.
        // The symlink `link` (pointing to `real`) must NOT be followed,
        // otherwise the files would be collected twice.
        assert_eq!(result.len(), 1, "expected 1 group from real/ subdir");
        let key = "take1_ (real)";
        let group = result.get(key).expect("missing group in real/ subdir");
        assert_eq!(group.len(), 1);
        // Also ensure the symlink was not followed: there should be no
        // group keyed as "take1_" at root level.
        assert!(
            !result.contains_key("take1_"),
            "symlinked dir content must not appear at root level"
        );
    }

    // ── Camera pattern regex tests ──────────────────────────────────────────

    #[test]
    fn test_sony_handycam_pattern_matches() {
        let re = Regex::new(r"^(?P<prefix>C\d{4})\.(?:mp4|MP4|MTS|mts)$").unwrap();
        assert!(re.is_match("C0001.MP4"));
        assert!(re.is_match("C0002.mp4"));
        assert!(re.is_match("C0123.MTS"));
        assert!(re.is_match("C9999.mts"));
        let caps = re.captures("C0042.MP4").unwrap();
        assert_eq!(caps.name("prefix").unwrap().as_str(), "C0042");
    }

    #[test]
    fn test_sony_handycam_pattern_rejects() {
        let re = Regex::new(r"^(?P<prefix>C\d{4})\.(?:mp4|MP4|MTS|mts)$").unwrap();
        assert!(!re.is_match("C00001.MP4"));   // 5 digits
        assert!(!re.is_match("C001.MP4"));      // 3 digits
        assert!(!re.is_match("D0001.MP4"));     // wrong prefix
        assert!(!re.is_match("C0001.AVI"));     // wrong extension
    }

    #[test]
    fn test_sony_fs100_pattern_matches() {
        let re = Regex::new(r"^(?P<prefix>\d{5})\.(?:MTS|mts)$").unwrap();
        assert!(re.is_match("00001.MTS"));
        assert!(re.is_match("12345.mts"));
        assert!(re.is_match("99999.MTS"));
        assert!(!re.is_match("00001.mp4"));
        assert!(!re.is_match("0001.MTS"));
    }

    #[test]
    fn test_canon_pattern_matches() {
        let re = Regex::new(r"^(?P<prefix>MVI_\d{4})\.(?:mp4|MP4)$").unwrap();
        assert!(re.is_match("MVI_0001.mp4"));
        assert!(re.is_match("MVI_9999.MP4"));
        let caps = re.captures("MVI_0123.mp4").unwrap();
        assert_eq!(caps.name("prefix").unwrap().as_str(), "MVI_0123");
        assert!(!re.is_match("MVI_00001.mp4"));
        assert!(!re.is_match("MVI_000.MP4"));
        assert!(!re.is_match("MVX_0001.mp4"));
    }

    #[test]
    fn test_panasonic_pattern_matches() {
        let re = Regex::new(r"^(?P<prefix>GH\d{5})\.(?:mp4|MP4)$").unwrap();
        assert!(re.is_match("GH00001.mp4"));
        assert!(re.is_match("GH12345.MP4"));
        let caps = re.captures("GH00001.mp4").unwrap();
        assert_eq!(caps.name("prefix").unwrap().as_str(), "GH00001");
        assert!(!re.is_match("GH0001.mp4"));
        assert!(!re.is_match("GH00001.mov"));
    }

    #[test]
    fn test_gopro_pattern_matches() {
        let re = Regex::new(r"^(?P<prefix>(?:GOPR\d{4}|GP\d{6}))\.(?:mp4|MP4)$").unwrap();
        assert!(re.is_match("GOPR0001.mp4"));
        assert!(re.is_match("GOPR9999.MP4"));
        assert!(re.is_match("GP000001.mp4"));
        assert!(re.is_match("GP123456.MP4"));
        let caps = re.captures("GOPR0042.mp4").unwrap();
        assert_eq!(caps.name("prefix").unwrap().as_str(), "GOPR0042");
        let caps = re.captures("GP000042.mp4").unwrap();
        assert_eq!(caps.name("prefix").unwrap().as_str(), "GP000042");
        assert!(!re.is_match("GOPR00001.mp4"));
        assert!(!re.is_match("GP00001.mp4"));
    }

    // ── match_files_all_patterns tests ──────────────────────────────────────

    #[test]
    fn test_match_all_empty_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        let result = match_files_all_patterns(dir.path());
        assert!(result.is_empty());
    }

    #[test]
    fn test_match_all_tascam_audio() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();
        std::fs::write(base.join("take1_S01.wav"), b"data").unwrap();
        std::fs::write(base.join("take1_S02.wav"), b"data").unwrap();
        std::fs::write(base.join("take2_S01.wav"), b"data").unwrap();

        let result = match_files_all_patterns(base);
        assert_eq!(result.len(), 2, "expected 2 TASCAM groups");
        for group in &result {
            assert_eq!(
                group.recording_type,
                crate::converter::RecordingType::MultiTrackAudio
            );
            assert_eq!(group.pattern_name, "TASCAM");
        }
    }

    #[test]
    fn test_match_all_sony_video_clips() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();
        std::fs::write(base.join("C0001.MP4"), b"data").unwrap();
        std::fs::write(base.join("C0002.MP4"), b"data").unwrap();
        std::fs::write(base.join("unrelated.txt"), b"text").unwrap();

        let result = match_files_all_patterns(base);
        assert_eq!(result.len(), 1, "expected 1 Sony group");
        let group = &result[0];
        assert_eq!(
            group.recording_type,
            crate::converter::RecordingType::VideoClipSequence
        );
        assert_eq!(group.files.len(), 2, "expected 2 video clips merged");
    }

    #[test]
    fn test_match_all_sony_fs100() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();
        std::fs::write(base.join("00001.MTS"), b"data").unwrap();
        std::fs::write(base.join("00002.MTS"), b"data").unwrap();
        std::fs::write(base.join("00003.MTS"), b"data").unwrap();

        let result = match_files_all_patterns(base);
        assert_eq!(result.len(), 1, "expected 1 FS100 group");
        let group = &result[0];
        assert_eq!(
            group.recording_type,
            crate::converter::RecordingType::VideoClipSequence
        );
        assert_eq!(group.files.len(), 3);
    }

    #[test]
    fn test_match_all_mixed_gear() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();
        // TASCAM audio
        std::fs::write(base.join("scene1_S01.wav"), b"data").unwrap();
        std::fs::write(base.join("scene1_S02.wav"), b"data").unwrap();
        // Sony video
        std::fs::write(base.join("C0001.MP4"), b"data").unwrap();
        std::fs::write(base.join("C0002.MP4"), b"data").unwrap();

        let result = match_files_all_patterns(base);
        assert_eq!(result.len(), 2, "expected 2 groups: TASCAM + Sony");
        let audio_groups: Vec<_> = result
            .iter()
            .filter(|g| g.recording_type == crate::converter::RecordingType::MultiTrackAudio)
            .collect();
        let video_groups: Vec<_> = result
            .iter()
            .filter(|g| g.recording_type == crate::converter::RecordingType::VideoClipSequence)
            .collect();
        assert_eq!(audio_groups.len(), 1);
        assert_eq!(video_groups.len(), 1);
    }

    #[test]
    fn test_match_all_gopro() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();
        std::fs::write(base.join("GOPR0001.mp4"), b"data").unwrap();
        std::fs::write(base.join("GOPR0002.mp4"), b"data").unwrap();

        let result = match_files_all_patterns(base);
        assert_eq!(result.len(), 1, "expected 1 GoPro group");
        assert_eq!(result[0].files.len(), 2);
    }

    #[test]
    fn test_match_all_ignores_unmatched_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();
        std::fs::write(base.join("readme.txt"), b"text").unwrap();
        std::fs::write(base.join("image.jpg"), b"img").unwrap();
        std::fs::write(base.join("data.bin"), b"bin").unwrap();

        let result = match_files_all_patterns(base);
        assert!(result.is_empty(), "no files should match any pattern");
    }

    // ── Recursive match_files_all_patterns tests ──────────────────────────

    #[test]
    fn test_recursive_all_patterns_nested_video() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();
        let sub = base.join("day1");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("C0001.MP4"), b"data").unwrap();
        std::fs::write(sub.join("C0002.MP4"), b"data").unwrap();

        let result = match_files_all_patterns(base);
        assert_eq!(result.len(), 1, "expected 1 Sony group in subdir");
        let group = &result[0];
        assert_eq!(group.rel_dir, "day1");
        assert_eq!(group.files.len(), 2);
    }

    #[test]
    fn test_recursive_all_patterns_camera_merge_within_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();
        let sub1 = base.join("card1");
        let sub2 = base.join("card2");
        std::fs::create_dir(&sub1).unwrap();
        std::fs::create_dir(&sub2).unwrap();
        // card1: C0001, C0002 → should merge into one group
        std::fs::write(sub1.join("C0001.MP4"), b"data").unwrap();
        std::fs::write(sub1.join("C0002.MP4"), b"data").unwrap();
        // card2: C0001, C0002 → separate group (different rel_dir)
        std::fs::write(sub2.join("C0001.MP4"), b"data").unwrap();
        std::fs::write(sub2.join("C0002.MP4"), b"data").unwrap();

        let result = match_files_all_patterns(base);
        assert_eq!(result.len(), 2, "expected 2 groups (one per card dir)");
        for group in &result {
            assert_eq!(group.files.len(), 2);
        }
    }

    #[test]
    fn test_recursive_all_patterns_mixed_gear_nested() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();
        // TASCAM audio in root
        std::fs::write(base.join("scene1_S01.wav"), b"data").unwrap();
        std::fs::write(base.join("scene1_S02.wav"), b"data").unwrap();
        // Sony video in subdir
        let sub = base.join("video");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("C0001.MP4"), b"data").unwrap();
        std::fs::write(sub.join("C0002.MP4"), b"data").unwrap();

        let result = match_files_all_patterns(base);
        assert_eq!(result.len(), 2, "expected 2 groups: TASCAM + Sony");
        let audio: Vec<_> = result
            .iter()
            .filter(|g| g.recording_type == crate::converter::RecordingType::MultiTrackAudio)
            .collect();
        let video: Vec<_> = result
            .iter()
            .filter(|g| g.recording_type == crate::converter::RecordingType::VideoClipSequence)
            .collect();
        assert_eq!(audio.len(), 1);
        assert_eq!(video.len(), 1);
        assert!(audio[0].rel_dir.is_empty());
        assert_eq!(video[0].rel_dir, "video");
    }

    #[test]
    fn test_recursive_all_patterns_skips_hidden() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();
        let hidden = base.join(".hidden");
        std::fs::create_dir(&hidden).unwrap();
        std::fs::write(hidden.join("take1_S01.wav"), b"data").unwrap();

        let result = match_files_all_patterns(base);
        assert!(result.is_empty());
    }

    #[test]
    fn test_recursive_all_patterns_skips_junk() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path();
        let junk = base.join("__MACOSX");
        std::fs::create_dir(&junk).unwrap();
        std::fs::write(junk.join("GOPR0001.MP4"), b"data").unwrap();

        let result = match_files_all_patterns(base);
        assert!(result.is_empty());
    }
}