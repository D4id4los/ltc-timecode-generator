use std::collections::BTreeMap;
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
                .name(pattern.prefix_group)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let _channel: u32 = match pattern.channel_group {
                Some(ch_name) => caps
                    .name(ch_name)
                    .and_then(|m| m.as_str().parse().ok())
                    .unwrap_or(0),
                None => 0,
            };

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
    pub files: Vec<PathBuf>,
    pub pattern_name: &'static str,
    pub recording_type: crate::converter::RecordingType,
}

/// Scan a folder with all built-in patterns (TASCAM + camera) simultaneously.
/// Files matching multiple patterns are assigned to the first match (TASCAM first).
/// Returns groups tagged with their `RecordingType`.
pub fn match_files_all_patterns(folder: &Path) -> Vec<MatchedGroup> {
    let mut used_stems: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut results: Vec<MatchedGroup> = Vec::new();

    // TASCAM pattern (audio) — limited to .wav only
    let audio_patterns = &BUILTIN_PATTERNS[..1];
    let all_patterns: Vec<&FileNamingPattern> = audio_patterns.iter().chain(CAMERA_PATTERNS.iter()).collect();

    for pattern in &all_patterns {
        let re = match Regex::new(pattern.regex) {
            Ok(r) => r,
            Err(_) => continue,
        };

        let dir = match folder.read_dir() {
            Ok(d) => d,
            Err(_) => continue,
        };

        let mut group_map: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();

        for entry in dir.flatten() {
            let path = entry.path();
            if !path.is_file() { continue; }

            let file_name = match path.file_name().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };

            // Track which form to use for dedup (stems for TASCAM, full name for cameras)
            let dedup_key = if pattern.name == "TASCAM" { stem.clone() } else { file_name.clone() };
            if used_stems.contains(&dedup_key) { continue; }

            // Try matching against the full file name (camera patterns include extension)
            // or against the stem (TASCAM pattern matches stem only)
            let match_str = if pattern.name == "TASCAM" { &stem } else { &file_name };
            if let Some(caps) = re.captures(match_str) {
                let prefix = caps.name(pattern.prefix_group).map(|m| m.as_str().to_string()).unwrap_or_default();
                used_stems.insert(dedup_key);
                group_map.entry(prefix).or_default().push(path);
            }
        }

        // Sort files in each group by the capture index (for TASCAM) or by sequential number (for cameras)
        for (prefix, mut files) in group_map {
            files.sort_by(|a, b| {
                let a_str = if pattern.name == "TASCAM" {
                    a.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string()
                } else {
                    a.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string()
                };
                let b_str = if pattern.name == "TASCAM" {
                    b.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string()
                } else {
                    b.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string()
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
                files,
                pattern_name: pattern.name,
                recording_type,
            });
        }
    }

    // Consecutive-numbering heuristic for video clips:
    // Group consecutive numbered files within the same camera pattern.
    // If C0001, C0002, C0003 are all separate matches with gaps of 1, merge them.
    // (This is already handled per-prefix by the regex capture groups above,
    //  since C(\d{4}) captures "C0001" and groups all by that prefix.)
    // Additional heuristic: group consecutive sequences by number suffix.
    let mut merged = Vec::new();
    let mut i = 0;
    while i < results.len() {
        let mut group = results[i].clone();
        // If this is a video clip sequence, try merging consecutive groups
        if group.recording_type == crate::converter::RecordingType::VideoClipSequence {
            let base_prefix = group.prefix.trim_end_matches(|c: char| c.is_ascii_digit()).to_string();
            let mut j = i + 1;
            while j < results.len() {
                let next = &results[j];
                if next.recording_type != crate::converter::RecordingType::VideoClipSequence { break; }
                let next_base = next.prefix.trim_end_matches(|c: char| c.is_ascii_digit()).to_string();
                if next_base != base_prefix { break; }
                // Merge consecutive groups
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
            assert_eq!(group.recording_type, crate::converter::RecordingType::MultiTrackAudio);
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
        assert_eq!(group.recording_type, crate::converter::RecordingType::VideoClipSequence);
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
        assert_eq!(group.recording_type, crate::converter::RecordingType::VideoClipSequence);
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
        let audio_groups: Vec<_> = result.iter().filter(|g| g.recording_type == crate::converter::RecordingType::MultiTrackAudio).collect();
        let video_groups: Vec<_> = result.iter().filter(|g| g.recording_type == crate::converter::RecordingType::VideoClipSequence).collect();
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
}
