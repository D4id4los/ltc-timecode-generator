use egui::{Color32, Stroke, Vec2, Visuals};

/// Accent color used for highlights, active buttons, clock digits glow.
pub const ACCENT: Color32 = Color32::from_rgb(0xFF, 0x5F, 0x1F);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Light,
}

impl Theme {
    pub fn toggle(self) -> Self {
        match self {
            Theme::Dark => Theme::Light,
            Theme::Light => Theme::Dark,
        }
    }

    pub fn apply(self, ctx: &egui::Context) {
        let colors = self.colors();
        let mut visuals = match self {
            Theme::Dark => Visuals::dark(),
            Theme::Light => Visuals::light(),
        };
        let border = colors.border_main;
        let text = colors.text_main;
        let text_title = colors.text_title;
        let text_muted = colors.text_muted;
        let nested = colors.nested_bg;
        let nested_hover = colors.nested_hover;

        visuals.panel_fill = colors.app_bg;
        visuals.window_fill = colors.card_bg;
        visuals.window_stroke = Stroke::new(1.0, border);
        visuals.faint_bg_color = nested;
        visuals.extreme_bg_color = colors.deep_bg;

        let w = &mut visuals.widgets;
        w.noninteractive.bg_fill = nested;
        w.noninteractive.fg_stroke = Stroke::new(1.0, text_muted);
        w.noninteractive.bg_stroke = Stroke::new(1.0, border);

        w.inactive.bg_fill = nested;
        w.inactive.weak_bg_fill = nested;
        w.inactive.fg_stroke = Stroke::new(1.0, text);

        w.hovered.bg_fill = nested_hover;
        w.hovered.weak_bg_fill = nested_hover;
        w.hovered.fg_stroke = Stroke::new(1.0, text_title);

        w.active.bg_fill = nested_hover;
        w.active.weak_bg_fill = nested_hover;
        w.active.fg_stroke = Stroke::new(1.0, ACCENT);

        w.open.bg_fill = colors.card_bg;

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
}

impl Theme {
    pub fn colors(self) -> ThemeColors {
        match self {
            Theme::Dark => ThemeColors {
                app_bg: Color32::from_rgb(0x0A, 0x0A, 0x0B),
                card_bg: Color32::from_rgb(0x1A, 0x1A, 0x1E),
                deep_bg: Color32::from_rgb(0x0A, 0x0A, 0x0B),
                nested_bg: Color32::from_rgb(0x14, 0x14, 0x16),
                nested_hover: Color32::from_rgb(0x1C, 0x1C, 0x20),
                text_main: Color32::from_rgb(0xE0, 0xE0, 0xE0),
                text_title: Color32::from_rgb(0xFF, 0xFF, 0xFF),
                text_muted: Color32::from_rgb(0x8E, 0x92, 0x99),
                text_secondary: Color32::from_rgb(0xCC, 0xCC, 0xCC),
                border_main: Color32::from_rgb(0x2A, 0x2A, 0x2E),
                btn_bg: Color32::from_rgb(0x1A, 0x1A, 0x1E),
                clock_sep: Color32::from_rgb(0x3F, 0x3F, 0x46),
            },
            Theme::Light => ThemeColors {
                app_bg: Color32::from_rgb(0xF4, 0xF4, 0xF6),
                card_bg: Color32::from_rgb(0xFF, 0xFF, 0xFF),
                deep_bg: Color32::from_rgb(0xEB, 0xEB, 0xEF),
                nested_bg: Color32::from_rgb(0xF4, 0xF4, 0xF6),
                nested_hover: Color32::from_rgb(0xE2, 0xE2, 0xE7),
                text_main: Color32::from_rgb(0x27, 0x27, 0x2A),
                text_title: Color32::from_rgb(0x09, 0x09, 0x0B),
                text_muted: Color32::from_rgb(0x71, 0x71, 0x7A),
                text_secondary: Color32::from_rgb(0x3F, 0x3F, 0x46),
                border_main: Color32::from_rgb(0xE4, 0xE4, 0xE7),
                btn_bg: Color32::from_rgb(0xFF, 0xFF, 0xFF),
                clock_sep: Color32::from_rgb(0xD4, 0xD4, 0xD8),
            },
        }
    }
}
