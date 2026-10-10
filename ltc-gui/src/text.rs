//! Central text-styling layer.
//!
//! egui 0.35 has no font-weight concept: `.strong()` only brightens the text
//! color, and italic is a geometric shear. Real bold/italic therefore comes
//! from explicitly selecting one of the named font families registered in
//! [`crate::theme::install_fonts`]. This module is the single home for that
//! resolution — call sites never touch `FontId`/`FontFamily` directly.
//!
//! ```ignore
//! ui.label(text(ui, "Hello").bold().color(ACCENT));
//! ui.label(text(ui, "/path/file.wav").mono());
//! ui.label(text(ui, "48 kHz").size(18.0).mono().bold());
//! ui.label(text(ui, "device").smallcaps().underline());
//! TextEdit::singleline(&mut s).font(text(ui, "").mono().font_id());
//! ```

use crate::theme::families;
use crate::theme::ThemeColors;
use egui::{Color32, FontFamily, FontId, RichText, Ui, WidgetText};
use std::sync::atomic::{AtomicU32, Ordering};

/// egui's default `TextStyle::Body` size (the app does not customize
/// `text_styles`); used by [`styled`] when no `Ui` is available to query.
pub const DEFAULT_BODY_SIZE: f32 = 13.0;

/// Width factor applied by [`StyledText::smallcaps`], which is simulated
/// (uppercase at reduced size) because egui cannot access OpenType features.
const SMALLCAPS_SCALE: f32 = 0.8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Weight {
    Regular,
    Bold,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slant {
    Upright,
    Italic,
}

/// Semantic text presets — the app's "CSS classes" for text. One variant per
/// recurring text appearance; the variant's doc comment names its role and
/// typical call sites. Resolve via [`StyledText::style`] and chain further
/// setters *after* it to override individual properties:
///
/// ```ignore
/// ui.label(text(ui, "Folder:").style(TextStyle::Label, &colors));
/// ui.label(text(ui, &route).style(TextStyle::MonoValue, &colors));
/// ui.label(text(ui, "ROLL").style(TextStyle::SequenceHeading, &colors));
/// // per-call overrides compose: fluid clock digits on the Display preset
/// ui.label(text(ui, "12:34").style(TextStyle::Display, &colors).size(d).color(c));
/// ```
///
/// Sizes resolve through the accessibility text scale
/// ([`set_text_scale`]); explicit `.size()`/`font()` calls never scale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextStyle {
    /// Form labels and field captions ("Folder:").
    Label,
    /// Tiny bold labels above grouped controls ("INTERFACE", "SAMPLE RATE").
    Caption,
    /// Muted small print: hint paragraphs, collapsible summaries.
    Description,
    /// Section headers and card titles.
    Heading,
    /// App-header title ("LTC ENGINE") and top-level brand text.
    Title,
    /// Card titles ("… - QUICK GUIDE") — one step below [`TextStyle::Title`].
    CardTitle,
    /// Navigation tab labels.
    TabHeading,
    /// Field legends above value rows ("ROLL", "SCENE", "TAKE").
    SequenceHeading,
    /// Status lines; the color is per-call (status/grade colors).
    Status,
    /// Big monospace digits: stepper readouts, clapper scene/take values.
    /// Color is per-call (title/accent).
    BigValue,
    /// Monospace labels: table headers, paths, file names.
    MonoLabel,
    /// Small monospace readouts and values.
    MonoValue,
    /// Monospace readout rows at the standard value size, default text
    /// color (LTC decode report values, dBFS preset buttons).
    MonoReadout,
    /// Monospace group headers.
    MonoHeading,
    /// Emphasized stat values ("48.0 KHZ").
    StatValue,
    /// Neutral note text (~10 px): hints, errors, warnings, blockers.
    /// Dynamic colors (error/warning/accent) are per-call `.color()`.
    Hint,
    /// Clock digits: fluid display text; size and color are per-call.
    Display,
}

/// One preset row: what [`StyledText::style`] applies.
#[derive(Clone, Copy)]
struct TextStyleSpec {
    size: Option<f32>,
    bold: bool,
    mono: bool,
    color: Option<ColorRole>,
}

/// Palette-backed color roles a preset can carry. Per-call dynamic colors
/// (status/grade/device) are never presets — they stay explicit `.color()`.
/// New roles are added here when a preset first needs them.
#[derive(Clone, Copy)]
enum ColorRole {
    Title,
    Muted,
}

impl ColorRole {
    fn resolve(self, colors: &ThemeColors) -> Color32 {
        match self {
            ColorRole::Title => colors.text_title,
            ColorRole::Muted => colors.text_muted,
        }
    }
}

impl TextStyle {
    /// Single home of the preset table — tweak values here and inspect the UI.
    fn spec(self) -> TextStyleSpec {
        use ColorRole as C;
        let (size, bold, mono, color) = match self {
            TextStyle::Label => (Some(10.0), false, false, Some(C::Muted)),
            TextStyle::Caption => (Some(8.0), true, false, Some(C::Muted)),
            TextStyle::Description => (Some(9.0), false, false, Some(C::Muted)),
            TextStyle::Heading => (Some(11.0), true, false, Some(C::Muted)),
            TextStyle::Title => (Some(16.0), true, false, Some(C::Title)),
            TextStyle::CardTitle => (Some(13.0), true, false, Some(C::Title)),
            TextStyle::TabHeading => (Some(12.0), true, false, None),
            TextStyle::SequenceHeading => (Some(9.0), true, false, Some(C::Muted)),
            TextStyle::Status => (Some(11.0), true, false, None),
            TextStyle::BigValue => (Some(18.0), true, true, None),
            TextStyle::MonoLabel => (Some(11.0), false, true, Some(C::Muted)),
            TextStyle::MonoValue => (Some(9.0), false, true, Some(C::Muted)),
            TextStyle::MonoReadout => (Some(10.0), false, true, None),
            TextStyle::MonoHeading => (Some(10.0), false, true, Some(C::Title)),
            TextStyle::StatValue => (Some(10.0), true, false, Some(C::Title)),
            TextStyle::Hint => (Some(10.0), false, false, Some(C::Muted)),
            TextStyle::Display => (None, true, true, None),
        };
        TextStyleSpec {
            size,
            bold,
            mono,
            color,
        }
    }

    /// Font-only view of the preset for widget helpers that take a
    /// [`FontSpec`] (badges, galleys — those own their text color). Presets
    /// without a table size fall back to the body size there.
    pub fn font_spec(self) -> FontSpec {
        let spec = self.spec();
        let mut f = font(spec.size.unwrap_or(DEFAULT_BODY_SIZE) * text_scale());
        if spec.bold {
            f = f.bold();
        }
        if spec.mono {
            f = f.mono();
        }
        f
    }
}

/// Global text scale in percent, applied at preset resolution only. Written
/// once per frame by the GUI from the engine-owned `text_scale_percent`.
static TEXT_SCALE_PERCENT: AtomicU32 = AtomicU32::new(100);

/// Set the accessibility text scale in percent (clamped 50..=400).
pub fn set_text_scale(percent: u32) {
    TEXT_SCALE_PERCENT.store(percent.clamp(50, 400), Ordering::Relaxed);
}

/// Serializes tests that mutate the process-global scale atomic; parallel
/// test threads would otherwise observe each other's temporary scale.
#[cfg(test)]
pub(crate) static SCALE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The text scale as a multiplier (1.0 = 100 %). Preset font sizes multiply
/// by this at resolution time; layout code that must keep pace with the
/// scaled text (content-column width, fluid size clamps) reads it directly.
pub fn text_scale() -> f32 {
    TEXT_SCALE_PERCENT.load(Ordering::Relaxed) as f32 / 100.0
}

/// How fixed-px layout thresholds (width gates, global spacing) track the
/// text scale. Deliberately a separate seam from [`text_scale`]: visual
/// review at extreme scales may show that gates need damped scaling —
/// change only this function, never the call sites.
pub fn layout_scale() -> f32 {
    text_scale()
}

/// A text string with optional styling, convertible into `RichText`
/// (for `ui.label`) or `WidgetText` (for `ui.button`). Every modifier is
/// optional; unset ones inherit the widget defaults.
#[derive(Clone, Debug)]
pub struct StyledText {
    s: String,
    default_size: f32,
    size: Option<f32>,
    weight: Weight,
    slant: Slant,
    mono: bool,
    underline: bool,
    color: Option<Color32>,
    smallcaps: bool,
}

/// Builder rooted in a `Ui`: the unset size inherits the `Ui`'s body size.
pub fn text(ui: &Ui, s: impl Into<String>) -> StyledText {
    StyledText::new(
        s,
        ui.style()
            .text_styles
            .get(&egui::TextStyle::Body)
            .map_or(DEFAULT_BODY_SIZE, |f| f.size),
    )
}

impl StyledText {
    fn new(s: impl Into<String>, default_size: f32) -> Self {
        Self {
            s: s.into(),
            default_size,
            size: None,
            weight: Weight::Regular,
            slant: Slant::Upright,
            mono: false,
            underline: false,
            color: None,
            smallcaps: false,
        }
    }

    pub fn bold(mut self) -> Self {
        self.weight = Weight::Bold;
        self
    }

    pub fn italic(mut self) -> Self {
        self.slant = Slant::Italic;
        self
    }

    pub fn mono(mut self) -> Self {
        self.mono = true;
        self
    }

    pub fn underline(mut self) -> Self {
        self.underline = true;
        self
    }

    pub fn size(mut self, size: f32) -> Self {
        self.size = Some(size);
        self
    }

    pub fn color(mut self, color: Color32) -> Self {
        self.color = Some(color);
        self
    }

    /// Simulated small caps: uppercase at reduced size.
    pub fn smallcaps(mut self) -> Self {
        self.smallcaps = true;
        self
    }

    /// Apply a text preset; chain further setters *after* it to override
    /// individual properties (`.style(TextStyle::Display, &colors).size(d)`).
    pub fn style(self, style: TextStyle, colors: &ThemeColors) -> Self {
        apply_spec(self, style.spec(), text_scale(), colors)
    }

    /// Effective rendered size (smallcaps scale applied).
    fn effective_size(&self) -> f32 {
        let base = self.size.unwrap_or(self.default_size);
        if self.smallcaps {
            base * SMALLCAPS_SCALE
        } else {
            base
        }
    }

    /// The resolved font for this style — escape hatch for text-bearing
    /// widgets that take a `FontId` directly (TextEdit, custom galleys).
    pub fn font_id(&self) -> FontId {
        FontId::new(
            self.effective_size(),
            resolve_family(self.mono, self.weight, self.slant),
        )
    }

    /// Explicit conversion to `RichText` (also available via `From`).
    pub fn rich(self) -> RichText {
        let font = self.font_id();
        let s = if self.smallcaps {
            self.s.to_uppercase()
        } else {
            self.s
        };
        let mut rich = RichText::new(s).font(font);
        if self.underline {
            rich = rich.underline();
        }
        if let Some(color) = self.color {
            rich = rich.color(color);
        }
        rich
    }
}

impl From<StyledText> for RichText {
    fn from(t: StyledText) -> RichText {
        t.rich()
    }
}

impl From<StyledText> for WidgetText {
    fn from(t: StyledText) -> WidgetText {
        WidgetText::from(t.rich())
    }
}

/// Pure core of [`StyledText::style`] — explicit scale keeps the preset
/// table tests deterministic regardless of the global atomic.
fn apply_spec(
    mut t: StyledText,
    spec: TextStyleSpec,
    scale: f32,
    colors: &ThemeColors,
) -> StyledText {
    if let Some(size) = spec.size {
        t.size = Some(size * scale);
    }
    if spec.bold {
        t.weight = Weight::Bold;
    }
    if spec.mono {
        t.mono = true;
    }
    if let Some(role) = spec.color {
        t.color = Some(role.resolve(colors));
    }
    t
}

/// Font styling without a string — for widget helpers that own their label
/// and resolve the text color themselves (badges, custom-painted buttons,
/// galleys). Same family matrix as [`StyledText`]; the size is required
/// because those helpers never inherit one.
#[derive(Clone, Copy, Debug)]
pub struct FontSpec {
    size: f32,
    weight: Weight,
    slant: Slant,
    mono: bool,
}

/// Start a [`FontSpec`] at an explicit size; chain `.bold()`, `.italic()`,
/// `.mono()` like on [`StyledText`].
pub fn font(size: f32) -> FontSpec {
    FontSpec {
        size,
        weight: Weight::Regular,
        slant: Slant::Upright,
        mono: false,
    }
}

impl FontSpec {
    pub fn bold(mut self) -> Self {
        self.weight = Weight::Bold;
        self
    }

    // Part of the styling API for parity with StyledText; no widget helper
    // composes bold/italic on a FontSpec yet.
    #[allow(dead_code)]
    pub fn italic(mut self) -> Self {
        self.slant = Slant::Italic;
        self
    }

    pub fn mono(mut self) -> Self {
        self.mono = true;
        self
    }

    pub fn font_id(&self) -> FontId {
        FontId::new(
            self.size,
            resolve_family(self.mono, self.weight, self.slant),
        )
    }

    /// Attach a string, yielding the full [`StyledText`] builder (for
    /// color/underline/presets on top).
    pub fn with(self, s: impl Into<String>) -> StyledText {
        StyledText {
            s: s.into(),
            default_size: DEFAULT_BODY_SIZE,
            size: Some(self.size),
            weight: self.weight,
            slant: self.slant,
            mono: self.mono,
            underline: false,
            color: None,
            smallcaps: false,
        }
    }
}

/// Single home of the (mono, weight, slant) → family matrix. There is no
/// mono bold-italic face registered; italic wins for that combo.
fn resolve_family(mono: bool, weight: Weight, slant: Slant) -> FontFamily {
    match (mono, weight, slant) {
        (false, Weight::Regular, Slant::Upright) => families::regular(),
        (false, Weight::Bold, Slant::Upright) => families::bold(),
        (false, Weight::Regular, Slant::Italic) => families::italic(),
        (false, Weight::Bold, Slant::Italic) => families::bold_italic(),
        (true, Weight::Regular, Slant::Upright) => families::mono(),
        (true, Weight::Bold, Slant::Upright) => families::mono_bold(),
        (true, _, Slant::Italic) => families::mono_italic(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Id, Ui, UiBuilder};

    fn make_ui(body_size: f32) -> Ui {
        let ctx = egui::Context::default();
        let mut style = (*ctx.global_style()).clone();
        style.text_styles.insert(
            egui::TextStyle::Body,
            FontId::new(body_size, FontFamily::Proportional),
        );
        ctx.set_global_style(style);
        crate::theme::install_fonts(&ctx);
        ctx.begin_pass(egui::RawInput::default());
        Ui::new(
            ctx,
            Id::new("test"),
            UiBuilder::new().max_rect(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::Vec2::splat(500.0),
            )),
        )
    }

    /// `text()` rooted in a default-size Ui, for tests without a real frame.
    fn st(s: impl Into<String>) -> StyledText {
        text(&make_ui(DEFAULT_BODY_SIZE), s)
    }

    fn galley_of(t: StyledText, ui: &Ui) -> std::sync::Arc<egui::Galley> {
        WidgetText::from(t).into_galley(
            ui,
            Some(egui::TextWrapMode::Extend),
            f32::INFINITY,
            egui::FontSelection::Default,
        )
    }

    fn vertex_colors(galley: &egui::Galley) -> Vec<Color32> {
        galley
            .rows
            .iter()
            .flat_map(|r| r.visuals.mesh.vertices.iter().map(|v| v.color))
            .collect()
    }

    #[test]
    fn family_matrix_is_exhaustive() {
        let base = st("x");
        let cases = [
            (base.clone(), families::regular()),
            (base.clone().bold(), families::bold()),
            (base.clone().italic(), families::italic()),
            (base.clone().bold().italic(), families::bold_italic()),
            (base.clone().mono(), families::mono()),
            (base.clone().mono().bold(), families::mono_bold()),
            (base.clone().mono().italic(), families::mono_italic()),
            (base.mono().bold().italic(), families::mono_italic()),
        ];
        for (t, expected) in cases {
            assert_eq!(t.font_id().family, expected, "wrong family for {t:?}");
        }
    }

    #[test]
    fn size_defaults_to_ui_body_size_and_is_overridable() {
        let ui = make_ui(11.5);
        assert_eq!(text(&ui, "x").font_id().size, 11.5);
        assert_eq!(text(&ui, "x").size(18.0).font_id().size, 18.0);
    }

    #[test]
    fn smallcaps_uppercases_and_shrinks() {
        assert_eq!(st("Device9").smallcaps().rich().text(), "DEVICE9");
        assert_eq!(
            st("x").smallcaps().font_id().size,
            DEFAULT_BODY_SIZE * SMALLCAPS_SCALE
        );
        // explicit size composes with the smallcaps scale
        assert_eq!(
            st("x").smallcaps().size(10.0).font_id().size,
            10.0 * SMALLCAPS_SCALE
        );
        // other modifiers keep working on top
        assert_eq!(
            st("x").smallcaps().bold().font_id().family,
            families::bold()
        );
        assert_eq!(
            st("x").smallcaps().mono().font_id().family,
            families::mono()
        );
    }

    #[test]
    fn underline_adds_non_glyph_mesh_vertices() {
        let ui = make_ui(DEFAULT_BODY_SIZE);
        let plain = galley_of(st("x"), &ui);
        let underlined = galley_of(st("x").underline(), &ui);
        let extra = |g: &egui::Galley| {
            g.rows
                .iter()
                .map(|r| r.visuals.mesh.vertices.len() - r.visuals.glyph_vertex_range.end)
                .sum::<usize>()
        };
        assert_eq!(extra(&plain), 0);
        assert!(extra(&underlined) > 0);
    }

    #[test]
    fn color_sets_mesh_vertex_color() {
        let ui = make_ui(DEFAULT_BODY_SIZE);
        let colors = vertex_colors(&galley_of(st("x").color(Color32::RED), &ui));
        assert!(!colors.is_empty());
        assert!(colors.iter().all(|c| *c == Color32::RED));
    }

    #[test]
    fn presets_resolve_size_family_and_color_for_both_themes() {
        for theme in [crate::theme::Theme::Dark, crate::theme::Theme::Light] {
            let colors = theme.colors();
            let cases = [
                (
                    TextStyle::Label,
                    Some(10.0),
                    false,
                    false,
                    Some(colors.text_muted),
                ),
                (
                    TextStyle::Caption,
                    Some(8.0),
                    true,
                    false,
                    Some(colors.text_muted),
                ),
                (
                    TextStyle::Description,
                    Some(9.0),
                    false,
                    false,
                    Some(colors.text_muted),
                ),
                (
                    TextStyle::Heading,
                    Some(11.0),
                    true,
                    false,
                    Some(colors.text_muted),
                ),
                (
                    TextStyle::Title,
                    Some(16.0),
                    true,
                    false,
                    Some(colors.text_title),
                ),
                (
                    TextStyle::CardTitle,
                    Some(13.0),
                    true,
                    false,
                    Some(colors.text_title),
                ),
                (TextStyle::TabHeading, Some(12.0), true, false, None),
                (
                    TextStyle::SequenceHeading,
                    Some(9.0),
                    true,
                    false,
                    Some(colors.text_muted),
                ),
                (TextStyle::Status, Some(11.0), true, false, None),
                (TextStyle::BigValue, Some(18.0), true, true, None),
                (
                    TextStyle::MonoLabel,
                    Some(11.0),
                    false,
                    true,
                    Some(colors.text_muted),
                ),
                (
                    TextStyle::MonoValue,
                    Some(9.0),
                    false,
                    true,
                    Some(colors.text_muted),
                ),
                (TextStyle::MonoReadout, Some(10.0), false, true, None),
                (
                    TextStyle::MonoHeading,
                    Some(10.0),
                    false,
                    true,
                    Some(colors.text_title),
                ),
                (
                    TextStyle::StatValue,
                    Some(10.0),
                    true,
                    false,
                    Some(colors.text_title),
                ),
                (
                    TextStyle::Hint,
                    Some(10.0),
                    false,
                    false,
                    Some(colors.text_muted),
                ),
                (TextStyle::Display, None, true, true, None),
            ];
            for (preset, size, bold, mono, color) in cases {
                let weight = if bold { Weight::Bold } else { Weight::Regular };
                let t = apply_spec(st("x"), preset.spec(), 1.0, &colors);
                assert_eq!(
                    t.font_id().family,
                    resolve_family(mono, weight, Slant::Upright),
                    "{preset:?} family ({theme:?})",
                );
                match size {
                    Some(s) => assert_eq!(t.font_id().size, s, "{preset:?} size ({theme:?})"),
                    None => assert_eq!(
                        t.font_id().size,
                        DEFAULT_BODY_SIZE,
                        "{preset:?} inherits body size ({theme:?})",
                    ),
                }
                if let Some(expected) = color {
                    let ui = make_ui(DEFAULT_BODY_SIZE);
                    let seen = vertex_colors(&galley_of(t, &ui));
                    assert!(!seen.is_empty(), "{preset:?} color ({theme:?})");
                    assert!(
                        seen.iter().all(|&c| c == expected),
                        "{preset:?} color ({theme:?})",
                    );
                }
            }
        }
    }

    #[test]
    fn preset_is_overridable_by_later_setters() {
        let colors = crate::theme::Theme::Dark.colors();
        let t = apply_spec(st("x"), TextStyle::Heading.spec(), 1.0, &colors)
            .size(16.0)
            .color(Color32::RED);
        assert_eq!(t.font_id().size, 16.0);
        let ui = make_ui(DEFAULT_BODY_SIZE);
        let seen = vertex_colors(&galley_of(t, &ui));
        assert!(!seen.is_empty());
        assert!(seen.iter().all(|&c| c == Color32::RED));

        // later weight/family setters upgrade on top of the preset
        let t = apply_spec(st("x"), TextStyle::Label.spec(), 1.0, &colors)
            .mono()
            .bold();
        assert_eq!(t.font_id().family, families::mono_bold());
    }

    #[test]
    fn font_spec_bridge_resolves_the_same_family_matrix() {
        for preset in [
            TextStyle::Label,
            TextStyle::Caption,
            TextStyle::Description,
            TextStyle::Heading,
            TextStyle::Title,
            TextStyle::CardTitle,
            TextStyle::TabHeading,
            TextStyle::SequenceHeading,
            TextStyle::Status,
            TextStyle::BigValue,
            TextStyle::MonoLabel,
            TextStyle::MonoValue,
            TextStyle::MonoReadout,
            TextStyle::MonoHeading,
            TextStyle::StatValue,
            TextStyle::Hint,
            TextStyle::Display,
        ] {
            let spec = preset.spec();
            let weight = if spec.bold {
                Weight::Bold
            } else {
                Weight::Regular
            };
            assert_eq!(
                preset.font_spec().font_id().family,
                resolve_family(spec.mono, weight, Slant::Upright),
                "{preset:?} font_spec family",
            );
            assert_eq!(
                preset.font_spec().font_id().size,
                spec.size.unwrap_or(DEFAULT_BODY_SIZE),
                "{preset:?} font_spec size",
            );
        }
    }

    #[test]
    fn text_scale_applies_at_preset_resolution_only_and_clamps() {
        let _lock = SCALE_LOCK.lock().unwrap();
        let ui = make_ui(DEFAULT_BODY_SIZE);
        let colors = crate::theme::Theme::Dark.colors();
        let before = TEXT_SCALE_PERCENT.load(Ordering::Relaxed);

        set_text_scale(150);
        assert_eq!(
            text(&ui, "x")
                .style(TextStyle::Label, &colors)
                .font_id()
                .size,
            15.0,
            "preset sizes scale",
        );
        assert_eq!(TextStyle::MonoValue.font_spec().font_id().size, 13.5);
        assert_eq!(
            text(&ui, "x").size(9.0).font_id().size,
            9.0,
            "raw sizes never scale",
        );

        set_text_scale(10_000);
        assert_eq!(TEXT_SCALE_PERCENT.load(Ordering::Relaxed), 400);
        set_text_scale(1);
        assert_eq!(TEXT_SCALE_PERCENT.load(Ordering::Relaxed), 50);
        set_text_scale(before);
    }

    #[test]
    fn converts_into_rich_text_and_widget_text() {
        let _: RichText = st("x").bold().into();
        let _: WidgetText = st("x").mono().into();
    }

    /// Guardrail: explicit `.size(N)` on text never scales with the text
    /// scale, so every fixed-size text site is a scaling bug waiting to
    /// happen. All text must resolve through a [`TextStyle`] preset (or a
    /// documented fluid-size exception). Allowlist entries carry the reason.
    #[test]
    fn no_fixed_size_text_sites_outside_the_allowlist() {
        let allow: &[(&str, &str)] = &[
            // This module: the `.size()` builder API itself, doc examples, tests.
            ("text.rs", "styling layer owns the API"),
            // Fluid clock digits: size derives from card width × text scale.
            ("clock.rs", "fluid digit sizes are scale-expressed"),
        ];
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        for entry in walk(&src) {
            let name = entry.file_name().unwrap().to_string_lossy().to_string();
            let rel = entry
                .strip_prefix(&src)
                .unwrap()
                .to_string_lossy()
                .to_string();
            if allow.iter().any(|(f, _)| *f == name) {
                continue;
            }
            for (i, line) in std::fs::read_to_string(&entry).unwrap().lines().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") {
                    continue;
                }
                // Only flag `.size(<number>)` — the fixed-size text setter —
                // not geometry reads like `galley.size().x`.
                if line.contains(".size(")
                    && line
                        .split(".size(")
                        .skip(1)
                        .any(|rest| rest.trim_start().starts_with(|c: char| c.is_ascii_digit()))
                {
                    offenders.push(format!("{rel}:{}", i + 1));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "fixed-size text sites bypass the TextStyle presets (add a preset or \
             allowlist with a reason): {offenders:?}"
        );
    }

    fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
        out.sort();
        out
    }

    #[test]
    fn font_spec_resolves_the_same_matrix() {
        use crate::text::font;
        assert_eq!(font(9.0).font_id().size, 9.0);
        assert_eq!(font(9.0).font_id().family, families::regular());
        assert_eq!(font(9.0).bold().font_id().family, families::bold());
        assert_eq!(font(9.0).italic().font_id().family, families::italic());
        assert_eq!(
            font(9.0).bold().italic().font_id().family,
            families::bold_italic()
        );
        assert_eq!(font(9.0).mono().font_id().family, families::mono());
        assert_eq!(
            font(9.0).mono().bold().font_id().family,
            families::mono_bold()
        );
        assert_eq!(
            font(9.0).mono().italic().font_id().family,
            families::mono_italic()
        );
        // spec → StyledText carries the resolved family and optional extras
        let t = font(11.0).mono().bold().with("x").underline();
        assert_eq!(t.font_id().family, families::mono_bold());
        assert_eq!(t.font_id().size, 11.0);
    }
}
