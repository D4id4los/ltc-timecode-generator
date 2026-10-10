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

    /// Muted small print (9 pt, secondary color) — the dominant recurring
    /// style for hints, captions and status detail lines.
    pub fn hint(self, colors: &ThemeColors) -> Self {
        self.size(9.0).color(colors.text_muted)
    }

    /// Title-colored text; the size stays unset because titles legitimately
    /// span several sizes at the call sites.
    pub fn title(self, colors: &ThemeColors) -> Self {
        self.color(colors.text_title)
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
    fn hint_preset_bundles_size_and_muted_color() {
        let ui = make_ui(DEFAULT_BODY_SIZE);
        let colors = crate::theme::Theme::Dark.colors();
        let t = st("x").hint(&colors);
        assert_eq!(t.font_id().size, 9.0);
        let colors_seen = vertex_colors(&galley_of(t, &ui));
        assert!(!colors_seen.is_empty());
        assert!(colors_seen.iter().all(|c| *c == colors.text_muted));
    }

    #[test]
    fn title_preset_sets_title_color_only() {
        let ui = make_ui(DEFAULT_BODY_SIZE);
        let colors = crate::theme::Theme::Dark.colors();
        let t = st("x").title(&colors);
        assert_eq!(t.font_id().size, DEFAULT_BODY_SIZE);
        let colors_seen = vertex_colors(&galley_of(t, &ui));
        assert!(!colors_seen.is_empty());
        assert!(colors_seen.iter().all(|c| *c == colors.text_title));
    }

    #[test]
    fn converts_into_rich_text_and_widget_text() {
        let _: RichText = st("x").bold().into();
        let _: WidgetText = st("x").mono().into();
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
