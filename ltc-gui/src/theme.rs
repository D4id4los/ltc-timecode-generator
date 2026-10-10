use egui::{Color32, FontData, FontFamily, Stroke, Vec2, Visuals};
use gui_engine::theme::{self, Rgb};
use std::sync::Arc;

fn to_color32(rgb: Rgb) -> Color32 {
    Color32::from_rgb(rgb.0, rgb.1, rgb.2)
}

/// Font-family names registered by [`install_fonts`]; [`crate::text`] resolves
/// (weight, slant, mono) combos onto these. Functions because
/// `FontFamily::Name` is not const-constructible.
pub mod families {
    use super::FontFamily;

    pub fn regular() -> FontFamily {
        FontFamily::Name("app-regular".into())
    }
    pub fn bold() -> FontFamily {
        FontFamily::Name("app-bold".into())
    }
    pub fn italic() -> FontFamily {
        FontFamily::Name("app-italic".into())
    }
    pub fn bold_italic() -> FontFamily {
        FontFamily::Name("app-bold-italic".into())
    }
    pub fn mono() -> FontFamily {
        FontFamily::Name("app-mono".into())
    }
    pub fn mono_bold() -> FontFamily {
        FontFamily::Name("app-mono-bold".into())
    }
    pub fn mono_italic() -> FontFamily {
        FontFamily::Name("app-mono-italic".into())
    }
}

/// Register the Roboto faces and expose them as named font families so
/// regular/bold/italic/monospace are all explicitly selectable (egui has no
/// font-weight concept). The egui defaults stay in every list as glyph
/// fallbacks so emoji etc. keep rendering.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    macro_rules! face {
        ($key:literal, $path:literal) => {
            fonts.font_data.insert(
                $key.to_owned(),
                Arc::new(FontData::from_owned(include_bytes!($path).to_vec())),
            );
        };
    }
    face!("roboto_regular", "../../assets/Roboto-Regular.ttf");
    face!("roboto_bold", "../../assets/Roboto-Bold.ttf");
    face!("roboto_italic", "../../assets/Roboto-Italic.ttf");
    face!("roboto_bold_italic", "../../assets/Roboto-BoldItalic.ttf");
    face!(
        "nerd_mono_regular",
        "../../assets/RobotoMonoNerdFontMono-Regular.ttf"
    );
    face!(
        "nerd_mono_bold",
        "../../assets/RobotoMonoNerdFontMono-Bold.ttf"
    );
    face!(
        "nerd_mono_italic",
        "../../assets/RobotoMonoNerdFontMono-Italic.ttf"
    );

    let default_prop = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let default_mono = fonts
        .families
        .get(&FontFamily::Monospace)
        .cloned()
        .unwrap_or_default();
    let prop_fallback = |names: &[&str]| -> Vec<String> {
        names
            .iter()
            .map(|n| (*n).to_owned())
            .chain(default_prop.iter().cloned())
            .collect()
    };
    let mono_fallback = |names: &[&str]| -> Vec<String> {
        names
            .iter()
            .map(|n| (*n).to_owned())
            .chain(default_mono.iter().cloned())
            .collect()
    };

    fonts.families.insert(
        families::regular(),
        prop_fallback(&["roboto_regular", "roboto_bold"]),
    );
    fonts
        .families
        .insert(families::bold(), prop_fallback(&["roboto_bold"]));
    fonts
        .families
        .insert(families::italic(), prop_fallback(&["roboto_italic"]));
    fonts.families.insert(
        families::bold_italic(),
        prop_fallback(&["roboto_bold_italic"]),
    );
    fonts.families.insert(
        families::mono(),
        mono_fallback(&["nerd_mono_regular", "nerd_mono_bold"]),
    );
    fonts.families.insert(
        families::mono_bold(),
        mono_fallback(&["nerd_mono_bold", "nerd_mono_regular"]),
    );
    fonts.families.insert(
        families::mono_italic(),
        mono_fallback(&["nerd_mono_italic", "nerd_mono_regular"]),
    );

    if let Some(family) = fonts.families.get_mut(&FontFamily::Proportional) {
        family.insert(0, "roboto_regular".to_owned());
        family.insert(1, "roboto_bold".to_owned());
    }
    if let Some(family) = fonts.families.get_mut(&FontFamily::Monospace) {
        family.insert(0, "nerd_mono_regular".to_owned());
        family.insert(1, "nerd_mono_bold".to_owned());
    }

    ctx.set_fonts(fonts);
}

/// Accent color used for highlights, active buttons, clock digits glow.
pub const ACCENT: Color32 = Color32::from_rgb(0xFF, 0x5F, 0x1F);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Light,
}

impl Theme {
    pub fn apply(self, ctx: &egui::Context) {
        let colors = self.colors();
        let border = colors.border_main;
        let text = colors.text_main;
        let text_title = colors.text_title;
        let text_muted = colors.text_muted;
        let nested = colors.nested_bg;
        let nested_hover = colors.nested_hover;

        let mut visuals = match self {
            Theme::Dark => Visuals::dark(),
            Theme::Light => Visuals::light(),
        };

        visuals.panel_fill = colors.app_bg;
        visuals.window_fill = colors.card_bg;
        visuals.window_stroke = Stroke::new(1.0, border);
        visuals.window_corner_radius = egui::CornerRadius::same(12);
        visuals.faint_bg_color = nested;
        visuals.extreme_bg_color = colors.deep_bg;

        let w = &mut visuals.widgets;
        w.noninteractive.bg_fill = nested;
        w.noninteractive.fg_stroke = Stroke::new(1.0, text_muted);
        w.noninteractive.bg_stroke = Stroke::new(1.0, border);
        w.noninteractive.corner_radius = egui::CornerRadius::same(12);

        w.inactive.bg_fill = nested;
        w.inactive.weak_bg_fill = nested;
        w.inactive.fg_stroke = Stroke::new(1.0, text);
        w.inactive.corner_radius = egui::CornerRadius::same(10);

        w.hovered.bg_fill = nested_hover;
        w.hovered.weak_bg_fill = nested_hover;
        w.hovered.fg_stroke = Stroke::new(1.0, text_title);
        w.hovered.corner_radius = egui::CornerRadius::same(10);

        w.active.bg_fill = nested_hover;
        w.active.weak_bg_fill = nested_hover;
        w.active.fg_stroke = Stroke::new(1.0, ACCENT);
        w.active.corner_radius = egui::CornerRadius::same(10);

        w.open.bg_fill = colors.card_bg;
        w.open.corner_radius = egui::CornerRadius::same(10);

        visuals.selection.bg_fill = ACCENT.linear_multiply(0.3);
        visuals.selection.stroke = Stroke::new(1.0, ACCENT);

        ctx.set_visuals(visuals);

        let mut style = (*ctx.global_style()).clone();
        style.spacing.button_padding = Vec2::new(10.0, 6.0);
        style.spacing.item_spacing = Vec2::new(8.0, 6.0);
        style.spacing.window_margin = egui::Margin::same(12);
        ctx.set_global_style(style);
    }
}

#[derive(Clone, Copy)]
#[allow(dead_code)]
pub struct ThemeColors {
    pub app_bg: Color32,
    pub card_bg: Color32,
    pub deep_bg: Color32,
    pub nested_bg: Color32,
    pub nested_hover: Color32,
    pub text_main: Color32,
    pub text_title: Color32,
    pub text_muted: Color32,
    pub text_secondary: Color32,
    pub border_main: Color32,
    pub btn_bg: Color32,
    pub clock_sep: Color32,
    pub error_red: Color32,
    pub warning_amber: Color32,
    pub success_green: Color32,
    pub info_blue: Color32,
}

impl Theme {
    pub fn colors(self) -> ThemeColors {
        let p = match self {
            Theme::Dark => &theme::DARK,
            Theme::Light => &theme::LIGHT,
        };
        ThemeColors {
            app_bg: to_color32(p.app_bg),
            card_bg: to_color32(p.card_bg),
            deep_bg: to_color32(p.deep_bg),
            nested_bg: to_color32(p.nested_bg),
            nested_hover: to_color32(p.nested_hover),
            text_main: to_color32(p.text_main),
            text_title: to_color32(p.text_title),
            text_muted: to_color32(p.text_muted),
            text_secondary: to_color32(p.text_secondary),
            border_main: to_color32(p.border_main),
            btn_bg: to_color32(p.btn_bg),
            clock_sep: to_color32(p.clock_sep),
            error_red: to_color32(p.error_red),
            warning_amber: to_color32(p.warning_amber),
            success_green: to_color32(p.success_green),
            info_blue: to_color32(p.info_blue),
        }
    }
}
