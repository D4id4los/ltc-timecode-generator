//! Persistent cache of *passing* HW-encoder validations.
//!
//! Core principle of the two-stage startup probe: **the cache accelerates
//! visibility, validation always runs and reconciles.** A cache hit lets
//! stage 1 publish a passing HW encoder immediately; the stage-2 validation
//! pass still test-encodes every HW candidate and prunes entries the cache
//! got wrong (driver updates, unplugged GPU, poisoned file). Failures are
//! never cached — a transient failure costs one re-probe per startup, not a
//! permanently broken encoder list.
//!
//! The cache key is (ffmpeg version, encoder list, hw devices): an exact
//! match means the same ffmpeg build saw the same devices last time. Driver
//! updates with an unchanged ffmpeg/devices key cannot be detected by the
//! key; they are bounded by the always-reconcile design — a stale pass is
//! visible at most until stage 2 of the current session prunes it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::converter::{FfmpegCapabilities, HwDeviceCapabilities};
use crate::video_codecs::{EncoderClass, HwFramePath, VIDEO_CODECS};

const CACHE_FILE: &str = "hw_encoder_cache.json";

/// Cache key: the exact-match identity of the environment the passes were
/// collected in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheKey {
    pub ffmpeg_version: String,
    pub encoders: BTreeSet<String>,
    pub hw: HwDeviceCapabilities,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HwValidationCache {
    pub key: CacheKey,
    pub passed: BTreeSet<String>,
}

/// The cache key for a caps value, or `None` when the ffmpeg version is
/// unknown (probe failure, test fakes) — a keyless caps can never be
/// cache-vouched or written back.
pub fn cache_key_for(caps: &FfmpegCapabilities) -> Option<CacheKey> {
    Some(CacheKey {
        ffmpeg_version: caps.ffmpeg_version.clone()?,
        encoders: caps.available_encoders.clone(),
        hw: caps.hw.clone(),
    })
}

fn is_hw_encoder(name: &str) -> bool {
    VIDEO_CODECS.iter().any(|codec| {
        codec
            .candidates
            .iter()
            .any(|c| c.class == EncoderClass::Hardware && c.name == name)
    })
}

/// Stage-1 publication: derive the caps to publish from the full (unvalidated)
/// probe result. Every HW encoder is withheld (an unvalidated encoder must
/// never be selectable) except those vouched for by a cache whose key exactly
/// matches this caps value. All non-HW encoders pass through untouched, so no
/// codec's encoder chain empties and no readiness blocker can appear.
pub fn publish_stage1_caps(
    full: &FfmpegCapabilities,
    cache: Option<&HwValidationCache>,
) -> FfmpegCapabilities {
    let vouched: BTreeSet<String> = match cache {
        Some(cache) => match cache_key_for(full) {
            Some(key) if key == cache.key => cache.passed.clone(),
            _ => BTreeSet::new(),
        },
        None => BTreeSet::new(),
    };
    let mut published = full.clone();
    let hw_names: Vec<String> = published
        .available_encoders
        .iter()
        .filter(|e| is_hw_encoder(e) && !vouched.contains(*e))
        .cloned()
        .collect();
    for name in hw_names {
        published.available_encoders.remove(&name);
    }
    published
}

/// Production entry point for the stage-2 reconciliation: run the full
/// validation pass over `caps` (which must carry the *full* encoder list),
/// then write the union of previously-passed (still key-valid) and
/// newly-passed encoders back to the cache. `load`/`save` are injectable for
/// tests; the production defaults read/write the cache file in the config dir.
#[allow(clippy::type_complexity)] // mirrors the seam signature in hw_device.rs
pub fn validate_hw_encoders_cached_with(
    mut caps: FfmpegCapabilities,
    probe: &mut dyn FnMut(&str, Option<HwFramePath>, Option<&str>) -> bool,
    load: &mut dyn FnMut() -> Option<HwValidationCache>,
    save: &mut dyn FnMut(&HwValidationCache),
) -> FfmpegCapabilities {
    // Key must be captured *before* validation mutates the encoder set —
    // it identifies the ffmpeg build, not the validation outcome.
    let key = cache_key_for(&caps);
    let mut passed: BTreeSet<String> = BTreeSet::new();
    let mut probed: BTreeSet<String> = BTreeSet::new();
    {
        let mut recording_probe =
            |name: &str, hw_frames: Option<HwFramePath>, vaapi_device: Option<&str>| {
                probed.insert(name.to_string());
                let ok = probe(name, hw_frames, vaapi_device);
                if ok {
                    passed.insert(name.to_string());
                }
                ok
            };
        crate::hw_device::validate_hw_encoders_with(
            &mut caps.available_encoders,
            &caps.hw,
            &mut recording_probe,
        );
    }

    if let Some(key) = key {
        let mut new_cache = HwValidationCache {
            key: key.clone(),
            passed: passed.clone(),
        };
        if let Some(prev) = load() {
            if prev.key == key {
                // Union of previously-passed (still key-valid) and
                // newly-passed encoders — but only previous passes that were
                // *not re-probed* this run; a re-probed candidate's fresh
                // result (including a failure) always wins. This keeps
                // "failures are never cached" absolute.
                for prev_pass in prev.passed {
                    if !probed.contains(&prev_pass) {
                        new_cache.passed.insert(prev_pass);
                    }
                }
            }
        }
        save(&new_cache);
    }
    caps
}

pub fn cache_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join(CACHE_FILE)
}

/// Load the cache file; a missing or corrupt file is an empty cache
/// (`None`, logged at debug — never an error surfaced to the probe).
pub fn load_hw_cache(cache_dir: &Path) -> Option<HwValidationCache> {
    let path = cache_path(cache_dir);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return None,
    };
    match serde_json::from_str(&content) {
        Ok(cache) => Some(cache),
        Err(e) => {
            log::debug!(
                "hw encoder cache at {} is corrupt, treating as empty: {}",
                path.display(),
                e
            );
            None
        }
    }
}

/// Persist the cache (temp file + atomic rename, mirroring config.rs style).
pub fn save_hw_cache(cache_dir: &Path, cache: &HwValidationCache) {
    let path = cache_path(cache_dir);
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            log::debug!("cannot create hw cache dir {}: {}", parent.display(), e);
            return;
        }
    }
    match serde_json::to_string_pretty(cache) {
        Ok(content) => {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, content).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
        Err(e) => log::debug!("cannot serialize hw encoder cache: {}", e),
    }
}

/// Default config-dir location of the cache (respects `XDG_CONFIG_HOME`
/// and the `LTC_CONFIG_HOME` test override, so engine tests are isolated
/// by construction on every platform).
pub fn cache_dir() -> Option<PathBuf> {
    crate::config::config_base_dir().map(|base| base.join(crate::config::CONFIG_DIR))
}

/// The production stage-2 closure wired into `EngineSeams`: validate all HW
/// candidates and reconcile the cache file in the config dir.
pub fn default_hw_validate(caps: FfmpegCapabilities) -> FfmpegCapabilities {
    let dir = cache_dir();
    validate_hw_encoders_cached_with(
        caps,
        &mut |name, hw_frames, vaapi_device| {
            crate::hw_device::test_encode("ffmpeg", name, hw_frames, vaapi_device)
        },
        &mut || dir.as_deref().and_then(load_hw_cache),
        &mut |cache| {
            if let Some(dir) = &dir {
                save_hw_cache(dir, cache);
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn caps_with(
        encoders: &[&str],
        version: Option<&str>,
        vaapi: Option<&str>,
    ) -> FfmpegCapabilities {
        FfmpegCapabilities {
            has_ffmpeg: true,
            available_encoders: encoders.iter().map(|s| s.to_string()).collect(),
            available_formats: BTreeSet::from(["mov".to_string(), "matroska".to_string()]),
            error_message: None,
            hw: HwDeviceCapabilities {
                vaapi_device: vaapi.map(|s| s.to_string()),
                vulkan_available: false,
            },
            ffmpeg_version: version.map(|s| s.to_string()),
        }
    }

    fn probe_always_ok(_name: &str, _hw: Option<HwFramePath>, _dev: Option<&str>) -> bool {
        true
    }

    fn probe_ok_except(
        failures: &'static [&'static str],
    ) -> impl FnMut(&str, Option<HwFramePath>, Option<&str>) -> bool {
        move |name: &str, _hw: Option<HwFramePath>, _dev: Option<&str>| !failures.contains(&name)
    }

    #[test]
    fn test_cache_hit_publishes_at_stage1() {
        // Matching-key cache vouches hevc_vaapi: it must survive stage-1
        // publication while other HW encoders are withheld — without any
        // probe call (pure function, no probe parameter).
        let full = caps_with(
            &["libx264", "hevc_vaapi", "h264_vaapi"],
            Some("v1"),
            Some("/dev/dri/renderD128"),
        );
        let key = cache_key_for(&full).unwrap();
        let cache = HwValidationCache {
            key,
            passed: BTreeSet::from(["hevc_vaapi".to_string()]),
        };
        let published = publish_stage1_caps(&full, Some(&cache));
        assert!(
            published.available_encoders.contains("hevc_vaapi"),
            "cache-vouched encoder must be published at stage 1"
        );
        assert!(
            !published.available_encoders.contains("h264_vaapi"),
            "non-vouched HW encoder must be withheld"
        );
        assert!(
            published.available_encoders.contains("libx264"),
            "non-HW encoders pass through"
        );
    }

    #[test]
    fn test_key_change_bypasses_cache() {
        // A different ffmpeg version must vouch nothing.
        let full = caps_with(
            &["libx264", "hevc_vaapi"],
            Some("v2"),
            Some("/dev/dri/renderD128"),
        );
        let key = CacheKey {
            ffmpeg_version: "v1".to_string(),
            encoders: full.available_encoders.clone(),
            hw: full.hw.clone(),
        };
        let cache = HwValidationCache {
            key,
            passed: BTreeSet::from(["hevc_vaapi".to_string()]),
        };
        let published = publish_stage1_caps(&full, Some(&cache));
        assert!(
            !published.available_encoders.contains("hevc_vaapi"),
            "key mismatch must bypass the cache"
        );
    }

    #[test]
    fn test_no_cache_withholds_all_hw() {
        let full = caps_with(&["libx264", "hevc_vaapi", "h264_vulkan"], Some("v1"), None);
        let published = publish_stage1_caps(&full, None);
        assert!(!published.available_encoders.contains("hevc_vaapi"));
        assert!(!published.available_encoders.contains("h264_vulkan"));
        assert!(published.available_encoders.contains("libx264"));
    }

    #[test]
    fn test_reconcile_prunes_stale_pass() {
        // Cache vouches hevc_vaapi, but the probe says it fails: it must be
        // removed from the reconciled caps AND from the saved cache.
        let caps = caps_with(
            &["libx264", "hevc_vaapi", "h264_vaapi"],
            Some("v1"),
            Some("/dev/dri/renderD128"),
        );
        let key = cache_key_for(&caps).unwrap();
        let stale = HwValidationCache {
            key: key.clone(),
            passed: BTreeSet::from(["hevc_vaapi".to_string()]),
        };
        let mut saved: Option<HwValidationCache> = None;
        let reconciled = validate_hw_encoders_cached_with(
            caps,
            &mut probe_ok_except(&["hevc_vaapi"]),
            &mut || Some(stale.clone()),
            &mut |c| saved = Some(c.clone()),
        );
        assert!(
            !reconciled.available_encoders.contains("hevc_vaapi"),
            "failing vouched encoder must be pruned from caps"
        );
        assert!(
            reconciled.available_encoders.contains("h264_vaapi"),
            "passing encoder must stay"
        );
        let saved = saved.expect("validation must write back the cache");
        assert!(
            !saved.passed.contains("hevc_vaapi"),
            "failing vouched encoder must be pruned from the saved cache"
        );
        assert!(saved.passed.contains("h264_vaapi"));
        assert_eq!(saved.key, key);
    }

    #[test]
    fn test_failure_not_cached() {
        // No prior cache; a failing candidate must end up in neither the
        // caps nor the saved cache.
        let caps = caps_with(
            &["libx264", "h264_vaapi"],
            Some("v1"),
            Some("/dev/dri/renderD128"),
        );
        let mut saved: Option<HwValidationCache> = None;
        let reconciled = validate_hw_encoders_cached_with(
            caps,
            &mut probe_ok_except(&["h264_vaapi"]),
            &mut || None,
            &mut |c| saved = Some(c.clone()),
        );
        assert!(!reconciled.available_encoders.contains("h264_vaapi"));
        let saved = saved.expect("validation must write back the cache");
        assert!(
            !saved.passed.contains("h264_vaapi"),
            "failures must never be cached"
        );
        assert!(
            !saved.passed.contains("libx264"),
            "non-HW encoders are not cache entries"
        );
        assert!(saved.passed.is_empty());
    }

    #[test]
    fn test_corrupt_cache_file_treated_as_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(cache_path(dir.path()), "{ not json !!!").unwrap();
        assert!(
            load_hw_cache(dir.path()).is_none(),
            "corrupt cache file must load as empty"
        );
        // And reconciliation with a corrupt cache simply runs full validation.
        let caps = caps_with(
            &["libx264", "h264_vaapi"],
            Some("v1"),
            Some("/dev/dri/renderD128"),
        );
        let mut probe_calls = 0usize;
        let reconciled = validate_hw_encoders_cached_with(
            caps,
            &mut |_n, _hw, _d| {
                probe_calls += 1;
                true
            },
            &mut || load_hw_cache(dir.path()),
            &mut |c| save_hw_cache(dir.path(), c),
        );
        assert_eq!(
            probe_calls, 1,
            "full validation must run on a corrupt cache"
        );
        assert!(reconciled.available_encoders.contains("h264_vaapi"));
        assert!(
            load_hw_cache(dir.path()).is_some(),
            "write-back must have replaced the corrupt file"
        );
    }

    #[test]
    fn test_writeback_merges_previous_passes() {
        // Previously passed X (same key); this run X passes again and Y is
        // newly observed: the write-back must be the union {X, Y}.
        let caps = caps_with(
            &["libx264", "h264_vaapi", "hevc_vaapi"],
            Some("v1"),
            Some("/dev/dri/renderD128"),
        );
        let key = cache_key_for(&caps).unwrap();
        let prev = HwValidationCache {
            key: key.clone(),
            passed: BTreeSet::from(["h264_vaapi".to_string()]),
        };
        let mut saved: Option<HwValidationCache> = None;
        let _ = validate_hw_encoders_cached_with(
            caps,
            &mut probe_always_ok, // both HW candidates pass this run
            &mut {
                let p = prev.clone();
                move || Some(p.clone())
            },
            &mut |c| saved = Some(c.clone()),
        );
        let saved = saved.expect("validation must write back the cache");
        assert!(
            saved.passed.contains("h264_vaapi"),
            "previous pass must survive the union"
        );
        assert!(
            saved.passed.contains("hevc_vaapi"),
            "new pass must be merged"
        );
        assert_eq!(saved.key, key);
    }

    #[test]
    fn test_missing_version_never_vouches_or_writes_back() {
        // Test fakes carry no ffmpeg version: no cache vouch at stage 1 and
        // no write-back after validation.
        let full = caps_with(
            &["libx264", "hevc_vaapi"],
            None,
            Some("/dev/dri/renderD128"),
        );
        assert!(cache_key_for(&full).is_none());
        let published = publish_stage1_caps(&full, None);
        assert!(!published.available_encoders.contains("hevc_vaapi"));

        let mut save_calls = 0usize;
        let _ =
            validate_hw_encoders_cached_with(full, &mut probe_always_ok, &mut || None, &mut |_c| {
                save_calls += 1
            });
        assert_eq!(
            save_calls, 0,
            "a versionless caps must not write back a cache"
        );
    }
}
