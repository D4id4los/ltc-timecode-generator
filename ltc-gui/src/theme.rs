use egui::{Color32, Stroke, Vec2, Visuals};
use gui_engine::theme::{self, Rgb};

fn to_color32(rgb: Rgb) -> Color32 {
    Color32::from_rgb(rgb.0, rgb.1, rgb.2)
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
