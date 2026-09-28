// ── Named-placeholder template for output filenames ─────────────────────
//
// Supports `{filename}`, `{device}`, `{clip}`, `{track}` and zero-padded
// variants `{clip:0Nd}` and `{track:0Nd}` where N = 1..=9 digits.

use std::fmt;

// ── Public types ─────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum Placeholder {
    Filename,
    Device,
    Clip,
    Track,
}

/// Context for expanding a name template.
#[derive(Clone, Debug)]
pub struct NamingContext {
    /// Source-file stem (without directory or extension).
    pub filename: String,
    /// Device name (human-readable, from camera metadata / filename pattern).
    pub device: String,
    /// Clip number (1-based; 1 for single-clip recordings).
    pub clip: usize,
    /// Track number (1-based output index; surviving order after LTC drop).
    pub track: usize,
}

/// Errors from parsing or validating a template string.
#[derive(Clone, Debug, PartialEq)]
pub enum TemplateError {
    UnknownPlaceholder(String),
    InvalidWidth,
    UnbalancedBraces,
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TemplateError::UnknownPlaceholder(p) => {
                write!(f, "Unknown placeholder '{}'. Allowed: {{filename}}, {{device}}, {{clip:0Nd}}, {{track:0Nd}}.", p)
            }
            TemplateError::InvalidWidth => {
                write!(f, "Placeholder width must be :0Nd where N is 1..9.")
            }
            TemplateError::UnbalancedBraces => {
                write!(f, "Unbalanced braces in template.")
            }
        }
    }
}

// ── Internal representation ─────────────────────────────────────────────

#[derive(Clone, Debug)]
enum Segment {
    Literal(String),
    Placeholder { name: Placeholder, width: Option<usize> },
}

/// A parsed output-filename template.
///
/// # Examples
///
/// ```
/// use gui_engine::naming::{NameTemplate, NamingContext};
///
/// let tmpl = NameTemplate::parse("{filename}_track{track:02d}").unwrap();
/// let ctx = NamingContext { filename: "session1".into(), device: "A6700".into(), clip: 1, track: 3 };
/// assert_eq!(tmpl.expand(&ctx), "session1_track03");
/// ```
#[derive(Clone, Debug)]
pub struct NameTemplate {
    segments: Vec<Segment>,
}

impl NameTemplate {
    /// Parse a template string. Returns an error on unknown placeholders,
    /// invalid width specifiers, or unbalanced braces.
    pub fn parse(input: &str) -> Result<Self, TemplateError> {
        let mut segments: Vec<Segment> = Vec::new();
        let mut remaining = input;

        while !remaining.is_empty() {
            if let Some(pos) = remaining.find('{') {
                if pos > 0 {
                    segments.push(Segment::Literal(remaining[..pos].to_string()));
                }
                let close = remaining[pos..].find('}').map(|p| pos + p);
                match close {
                    None => return Err(TemplateError::UnbalancedBraces),
                    Some(end) => {
                        let inner = &remaining[pos + 1..end];
                        let (name, width) = parse_placeholder(inner)?;
                        segments.push(Segment::Placeholder { name, width });
                        remaining = &remaining[end + 1..];
                    }
                }
            } else {
                segments.push(Segment::Literal(remaining.to_string()));
                break;
            }
        }

        Ok(NameTemplate { segments })
    }

    /// Expand this template using the given context.
    pub fn expand(&self, ctx: &NamingContext) -> String {
        let mut out = String::new();
        for seg in &self.segments {
            match seg {
                Segment::Literal(s) => out.push_str(s),
                Segment::Placeholder { name, width } => match name {
                    Placeholder::Filename => out.push_str(&ctx.filename),
                    Placeholder::Device => out.push_str(&ctx.device),
                    Placeholder::Clip => {
                        push_padded(&mut out, ctx.clip, *width);
                    }
                    Placeholder::Track => {
                        push_padded(&mut out, ctx.track, *width);
                    }
                },
            }
        }
        out
    }
}

// ── Default templates ───────────────────────────────────────────────────

pub const DEFAULT_PREFIX: &str = "{filename}";
pub const DEFAULT_AUDIO_SUFFIX: &str = "_clip{clip:01d}_tr{track:01d}";
pub const DEFAULT_VIDEO_SUFFIX: &str = "_clip{clip:01d}";

// ── Helpers ─────────────────────────────────────────────────────────────

fn parse_placeholder(inner: &str) -> Result<(Placeholder, Option<usize>), TemplateError> {
    // {filename} — no width suffix
    if inner == "filename" {
        return Ok((Placeholder::Filename, None));
    }

    // {device} — no width suffix; reject any width specifier.
    if let Some(rest) = inner.strip_prefix("device") {
        if rest.is_empty() {
            return Ok((Placeholder::Device, None));
        }
        return Err(TemplateError::InvalidWidth);
    }

    // {clip} or {clip:0Nd}
    if let Some(rest) = inner.strip_prefix("clip") {
        let width = parse_width(rest)?;
        return Ok((Placeholder::Clip, width));
    }

    // {track} or {track:0Nd}
    if let Some(rest) = inner.strip_prefix("track") {
        let width = parse_width(rest)?;
        return Ok((Placeholder::Track, width));
    }

    Err(TemplateError::UnknownPlaceholder(inner.to_string()))
}

fn parse_width(s: &str) -> Result<Option<usize>, TemplateError> {
    if s.is_empty() {
        return Ok(None);
    }
    if let Some(rest) = s.strip_prefix(":0") {
        let mut chars = rest.chars();
        match (chars.next(), chars.next()) {
            (Some(d), Some('d')) if d.is_ascii_digit() && d != '0' => {
                let n: usize = d.to_digit(10).unwrap() as usize;
                Ok(Some(n))
            }
            _ => Err(TemplateError::InvalidWidth),
        }
    } else {
        Err(TemplateError::InvalidWidth)
    }
}

fn push_padded(out: &mut String, value: usize, width: Option<usize>) {
    match width {
        Some(w) => {
            let s = format!("{:0width$}", value, width = w);
            out.push_str(&s);
        }
        None => out.push_str(&value.to_string()),
    }
}

/// Validate a template string. Returns an error if parsing fails.
pub fn validate_template(input: &str) -> Result<(), TemplateError> {
    NameTemplate::parse(input).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Parsing tests ──────────────────────────────────────────────────

    #[test]
    fn test_parse_literal_only() {
        let t = NameTemplate::parse("hello").unwrap();
        assert_eq!(t.segments.len(), 1);
        assert!(matches!(&t.segments[0], Segment::Literal(s) if s == "hello"));
    }

    #[test]
    fn test_parse_empty() {
        let t = NameTemplate::parse("").unwrap();
        assert!(t.segments.is_empty());
    }

    #[test]
    fn test_parse_filename() {
        let t = NameTemplate::parse("{filename}").unwrap();
        assert_eq!(t.segments.len(), 1);
        match &t.segments[0] {
            Segment::Placeholder { name: Placeholder::Filename, width: None } => {}
            _ => panic!("expected Filename placeholder"),
        }
    }

    #[test]
    fn test_parse_clip_bare() {
        let t = NameTemplate::parse("{clip}").unwrap();
        match &t.segments[0] {
            Segment::Placeholder { name: Placeholder::Clip, width: None } => {}
            _ => panic!("expected Clip placeholder"),
        }
    }

    #[test]
    fn test_parse_clip_padded() {
        let t = NameTemplate::parse("{clip:02d}").unwrap();
        match &t.segments[0] {
            Segment::Placeholder { name: Placeholder::Clip, width: Some(2) } => {}
            _ => panic!("expected Clip placeholder with width 2"),
        }
    }

    #[test]
    fn test_parse_track_padded() {
        let t = NameTemplate::parse("{track:03d}").unwrap();
        match &t.segments[0] {
            Segment::Placeholder { name: Placeholder::Track, width: Some(3) } => {}
            _ => panic!("expected Track placeholder with width 3"),
        }
    }

    #[test]
    fn test_parse_device() {
        let t = NameTemplate::parse("{device}").unwrap();
        assert_eq!(t.segments.len(), 1);
        match &t.segments[0] {
            Segment::Placeholder { name: Placeholder::Device, width: None } => {}
            _ => panic!("expected Device placeholder"),
        }
    }

    #[test]
    fn test_parse_device_width_rejected() {
        let err = NameTemplate::parse("{device:01d}").unwrap_err();
        assert_eq!(err, TemplateError::InvalidWidth);
    }

    #[test]
    fn test_parse_multiple_placeholders() {
        let t = NameTemplate::parse("{filename}_clip{clip:01d}_tr{track:02d}").unwrap();
        assert_eq!(t.segments.len(), 5);
    }

    #[test]
    fn test_parse_mixed_literal_and_placeholder() {
        let t = NameTemplate::parse("rec_{clip:01d}.wav").unwrap();
        assert_eq!(t.segments.len(), 3);
        match &t.segments[1] {
            Segment::Placeholder { name: Placeholder::Clip, width: Some(1) } => {}
            _ => panic!("expected Clip placeholder"),
        }
    }

    // ── Error tests ─────────────────────────────────────────────────────

    #[test]
    fn test_parse_unknown_placeholder() {
        let err = NameTemplate::parse("{foo}").unwrap_err();
        assert_eq!(err, TemplateError::UnknownPlaceholder("foo".into()));
    }

    #[test]
    fn test_parse_unknown_in_mixed() {
        let err = NameTemplate::parse("prefix_{bar}_suffix").unwrap_err();
        assert_eq!(err, TemplateError::UnknownPlaceholder("bar".into()));
    }

    #[test]
    fn test_parse_unbalanced_braces() {
        let err = NameTemplate::parse("{filename").unwrap_err();
        assert_eq!(err, TemplateError::UnbalancedBraces);
    }

    #[test]
    fn test_parse_invalid_width() {
        let err = NameTemplate::parse("{clip:3d}").unwrap_err();
        assert_eq!(err, TemplateError::InvalidWidth);
    }

    #[test]
    fn test_parse_width_zero() {
        let err = NameTemplate::parse("{clip:00d}").unwrap_err();
        assert_eq!(err, TemplateError::InvalidWidth);
    }

    #[test]
    fn test_parse_width_over_9() {
        let err = NameTemplate::parse("{track:010d}").unwrap_err();
        assert_eq!(err, TemplateError::InvalidWidth);
    }

    #[test]
    fn test_parse_empty_inner() {
        let err = NameTemplate::parse("{}").unwrap_err();
        assert_eq!(err, TemplateError::UnknownPlaceholder("".into()));
    }

    // ── Expansion tests ─────────────────────────────────────────────────

    fn ctx() -> NamingContext {
        NamingContext {
            filename: "recording".into(),
            device: "A6700".into(),
            clip: 5,
            track: 3,
        }
    }

    #[test]
    fn test_expand_literal() {
        let t = NameTemplate::parse("hello").unwrap();
        assert_eq!(t.expand(&ctx()), "hello");
    }

    #[test]
    fn test_expand_filename() {
        let t = NameTemplate::parse("{filename}").unwrap();
        assert_eq!(t.expand(&ctx()), "recording");
    }

    #[test]
    fn test_expand_clip_bare() {
        let t = NameTemplate::parse("{clip}").unwrap();
        assert_eq!(t.expand(&ctx()), "5");
    }

    #[test]
    fn test_expand_clip_padded() {
        let t = NameTemplate::parse("{clip:03d}").unwrap();
        assert_eq!(t.expand(&ctx()), "005");
    }

    #[test]
    fn test_expand_track_bare() {
        let t = NameTemplate::parse("{track}").unwrap();
        assert_eq!(t.expand(&ctx()), "3");
    }

    #[test]
    fn test_expand_track_padded() {
        let t = NameTemplate::parse("{track:02d}").unwrap();
        assert_eq!(t.expand(&ctx()), "03");
    }

    #[test]
    fn test_expand_device() {
        let t = NameTemplate::parse("{device}").unwrap();
        assert_eq!(t.expand(&ctx()), "A6700");
    }

    #[test]
    fn test_expand_device_empty() {
        let t = NameTemplate::parse("{device}").unwrap();
        let c = NamingContext { filename: "x".into(), device: "".into(), clip: 1, track: 1 };
        assert_eq!(t.expand(&c), "");
    }

    #[test]
    fn test_expand_device_in_path() {
        let t = NameTemplate::parse("{device}_{filename}").unwrap();
        assert_eq!(t.expand(&ctx()), "A6700_recording");
    }

    #[test]
    fn test_expand_full_path() {
        let t = NameTemplate::parse("{filename}_clip{clip:01d}_tr{track:02d}").unwrap();
        assert_eq!(t.expand(&ctx()), "recording_clip5_tr03");
    }

    #[test]
    fn test_expand_with_literals() {
        let t = NameTemplate::parse("prefix_{clip}_suffix").unwrap();
        assert_eq!(t.expand(&ctx()), "prefix_5_suffix");
    }

    #[test]
    fn test_expand_empty() {
        let t = NameTemplate::parse("").unwrap();
        assert_eq!(t.expand(&ctx()), "");
    }

    // ── Edge cases ──────────────────────────────────────────────────────

    #[test]
    fn test_expand_track_at_1() {
        let t = NameTemplate::parse("{track:03d}").unwrap();
        let c = NamingContext { filename: "x".into(), device: "A6700".into(), clip: 1, track: 1 };
        assert_eq!(t.expand(&c), "001");
    }

    #[test]
    fn test_expand_large_clip() {
        let t = NameTemplate::parse("{clip:02d}").unwrap();
        let c = NamingContext { filename: "x".into(), device: "A6700".into(), clip: 42, track: 1 };
        // width=2 but value needs 2 chars — fits
        assert_eq!(t.expand(&c), "42");
    }

    #[test]
    fn test_expand_clip_wider_than_width() {
        let t = NameTemplate::parse("{clip:02d}").unwrap();
        let c = NamingContext { filename: "x".into(), device: "A6700".into(), clip: 142, track: 1 };
        // width=2 but value needs 3 — still produces "142" (no truncation)
        assert_eq!(t.expand(&c), "142");
    }

    #[test]
    fn test_expand_filename_with_extension() {
        let t = NameTemplate::parse("{filename}").unwrap();
        let c = NamingContext { filename: "C0001.MP4".into(), device: "A6700".into(), clip: 1, track: 1 };
        // filename includes "extension" since it's just the stem
        assert_eq!(t.expand(&c), "C0001.MP4");
    }

    // ── Consecutive braces test ──────────────────────────────────────────

    #[test]
    fn test_consecutive_placeholders() {
        let t = NameTemplate::parse("{clip}{track}").unwrap();
        let c = NamingContext { filename: "x".into(), device: "A6700".into(), clip: 1, track: 2 };
        assert_eq!(t.expand(&c), "12");
    }

    // ── validate_template convenience ────────────────────────────────────

    #[test]
    fn test_validate_template_ok() {
        assert!(validate_template("{filename}_track{track:01d}").is_ok());
    }

    #[test]
    fn test_validate_template_err() {
        assert!(validate_template("{unknown}").is_err());
    }

    // ── Display ──────────────────────────────────────────────────────────

    #[test]
    fn test_error_display() {
        let e = TemplateError::UnknownPlaceholder("foo".into());
        let msg = e.to_string();
        assert!(msg.contains("foo"));
        assert!(msg.contains("filename"));
    }

    #[test]
    fn test_defaults() {
        // Verify constants parse correctly
        assert!(NameTemplate::parse(DEFAULT_PREFIX).is_ok());
        assert!(NameTemplate::parse(DEFAULT_AUDIO_SUFFIX).is_ok());
        assert!(NameTemplate::parse(DEFAULT_VIDEO_SUFFIX).is_ok());
    }
}