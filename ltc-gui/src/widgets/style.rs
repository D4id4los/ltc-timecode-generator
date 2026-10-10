//! Shared visual grammar for every interactive element in ltc-gui.
//!
//! Interaction groups — and only interaction groups — decide how an
//! element looks. The taxonomy (hard rules):
//!
//! - **Static** elements are never clickable-looking: tinted/neutral
//!   chips without full strokes, no hover, `Sense::hover()` only.
//! - **Clickable** elements always show an obvious hover reaction and
//!   the pointing-hand cursor.
//! - **Selected** options are a *state*, not a button: accent-tinted
//!   fill + accent stroke, never the solid fill of a primary action.
//! - **Primary / Danger / Success / Muted** actions are solid-filled
//!   buttons that lighten on hover and darken while pressed.
//!
//! Elements of one group look the same (importance scales size only);
//! elements of different groups never look the same.
//!
//! egui 0.35 note: `Button::fill()` overrides all hover effects, so the
//! filled builders here are custom-painted from `Response::hovered()` /
//! `is_pointer_button_down_on()` (both known before painting).

use crate::text::{text, FontSpec, TextStyle};
use egui::{Color32, Sense, Stroke, TextWrapMode, Ui, Vec2};

use crate::theme::{ThemeColors, ACCENT};

/// Danger-action fill (STOP / CANCEL family).
pub const DANGER_RED: Color32 = Color32::from_rgb(0xDC, 0x26, 0x26);
/// Success-action fill (transport START).
pub const SUCCESS_GREEN: Color32 = Color32::from_rgb(0x22, 0xC5, 0x5E);

/// Solid-fill action variants. Importance is expressed by size only.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ActionStyle {
    /// The one main action of a view (CONVERT, Detect LTC, CLAP & BEEP).
    Primary,
    /// Destructive / aborting action (STOP, CANCEL).
    Danger,
    /// Positive transport action (START).
    Success,
    /// Secondary action (Reset, Lock, header utilities).
    Muted,
}

/// Interaction state of a one-of-many option element.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OptionState {
    /// Clickable, not hovered.
    Idle,
    /// Clickable and hovered — obvious affordance signal.
    Hover,
    /// Currently active; re-clicking is a no-op, so no hover styling.
    Selected,
    /// Not clickable (e.g. while the stream is playing).
    Disabled,
}

/// Static badge semantic tone (never clickable). Variants are added when
/// a caller needs them — no speculative tones.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BadgeTone {
    Warning,
    Danger,
    Accent,
}

/// Colors for one option element state (chips, cards).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct OptionVisuals {
    pub fill: Color32,
    pub stroke: Stroke,
    pub text: Color32,
    pub strong: bool,
}

/// Colors for a solid-fill action button state.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ActionVisuals {
    pub fill: Color32,
    pub text: Color32,
}

/// Ring + core colors for one channel-matrix radio dot.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct MatrixCellVisuals {
    pub ring: Stroke,
    pub core: Color32,
}

fn blend(a: Color32, b: Color32, t: f32) -> Color32 {
    a.lerp_to_gamma(b, t)
}

fn lighten(c: Color32, t: f32) -> Color32 {
    blend(c, Color32::WHITE, t)
}

fn darken(c: Color32, t: f32) -> Color32 {
    blend(c, Color32::BLACK, t)
}

/// Ad-hoc hover tint for custom surfaces that must stay clickable-looking
/// (clapper board, hamburger) without becoming full widgets.
pub fn hover_fill(base: Color32, hovered: bool) -> Color32 {
    if hovered {
        lighten(base, 0.15)
    } else {
        base
    }
}

/// Visual decoder for one-of-many option elements. `Selected` is
/// deliberately distinct from any [`ActionStyle`]: tinted fill, accent
/// stroke, accent text — a state, not a button.
pub fn option_visuals(colors: &ThemeColors, state: OptionState) -> OptionVisuals {
    match state {
        OptionState::Idle => OptionVisuals {
            fill: colors.deep_bg,
            stroke: Stroke::new(1.0, colors.border_main),
            text: colors.text_title,
            strong: false,
        },
        OptionState::Hover => OptionVisuals {
            fill: ACCENT.linear_multiply(0.08),
            stroke: Stroke::new(1.5, ACCENT.linear_multiply(0.7)),
            text: ACCENT,
            strong: false,
        },
        OptionState::Selected => OptionVisuals {
            fill: ACCENT.linear_multiply(0.18),
            stroke: Stroke::new(1.75, ACCENT),
            text: ACCENT,
            strong: true,
        },
        OptionState::Disabled => OptionVisuals {
            fill: blend(colors.deep_bg, colors.app_bg, 0.5),
            stroke: Stroke::new(1.0, blend(colors.border_main, colors.app_bg, 0.4)),
            text: colors.text_muted,
            strong: false,
        },
    }
}

/// Visual decoder for solid-fill action buttons. Hover lightens, press
/// darkens; disabled is dimmed with muted text.
pub fn filled_action_visuals(
    colors: &ThemeColors,
    style: ActionStyle,
    hovered: bool,
    pressed: bool,
    enabled: bool,
) -> ActionVisuals {
    let (base_fill, text) = match style {
        ActionStyle::Primary => (ACCENT, Color32::BLACK),
        ActionStyle::Danger => (DANGER_RED, Color32::WHITE),
        ActionStyle::Success => (SUCCESS_GREEN, Color32::BLACK),
        ActionStyle::Muted => (colors.nested_bg, colors.text_title),
    };
    let fill = if !enabled {
        blend(base_fill, colors.app_bg, 0.5)
    } else if pressed {
        darken(base_fill, 0.12)
    } else if hovered {
        match style {
            ActionStyle::Muted => colors.nested_hover,
            _ => lighten(base_fill, 0.15),
        }
    } else {
        base_fill
    };
    let text = if enabled { text } else { colors.text_muted };
    ActionVisuals { fill, text }
}

/// Visual decoder for channel-matrix radio dots. Selected has the fat
/// accent ring; hover is a clear mid-step between idle and selected.
pub fn matrix_cell_visuals(
    colors: &ThemeColors,
    selected: bool,
    hovered: bool,
    ltc_row: bool,
) -> MatrixCellVisuals {
    let (ring, core) = if selected {
        (Stroke::new(2.5, ACCENT), ACCENT.linear_multiply(0.3))
    } else if hovered {
        (
            Stroke::new(1.5, ACCENT.linear_multiply(0.7)),
            ACCENT.linear_multiply(0.15),
        )
    } else if ltc_row {
        (
            Stroke::new(1.0, ACCENT.linear_multiply(0.5)),
            ACCENT.linear_multiply(0.12),
        )
    } else {
        (Stroke::new(1.0, colors.border_main), colors.deep_bg)
    };
    MatrixCellVisuals { ring, core }
}

/// (fill, stroke, text) for a static semantic badge.
pub fn badge_colors(colors: &ThemeColors, tone: BadgeTone) -> (Color32, Stroke, Color32) {
    match tone {
        BadgeTone::Warning => (
            colors.warning_amber.linear_multiply(0.12),
            Stroke::new(0.5, colors.warning_amber.linear_multiply(0.3)),
            colors.warning_amber,
        ),
        BadgeTone::Danger => (
            colors.error_red.linear_multiply(0.08),
            Stroke::new(0.5, colors.error_red.linear_multiply(0.3)),
            colors.error_red,
        ),
        BadgeTone::Accent => (
            ACCENT.linear_multiply(0.10),
            Stroke::new(1.0, ACCENT.linear_multiply(0.5)),
            ACCENT,
        ),
    }
}

/// The outer content panel shared by all four tabs.
pub fn content_frame(colors: &ThemeColors) -> egui::Frame {
    egui::Frame::new()
        .fill(colors.card_bg)
        .corner_radius(12.0)
        .stroke(Stroke::new(1.5, colors.border_main))
        .inner_margin(egui::Margin::same(16))
}

/// The standard tab content area: the shared [`content_frame`] panel pinned
/// to the full width of the deck row — the same width the header, clock and
/// status bar span. egui frames shrink-wrap to their widest child, so
/// without the min-width pin each tab's card ended up only as wide as its
/// content (the offload card rendered visibly narrower than the header).
/// Contents are laid out vertically, left-aligned.
///
/// The returned [`egui::InnerResponse`] is the *frame's* — `.response` carries
/// the full panel rect, `.inner` the closure's value.
pub fn content_area<R>(
    ui: &mut Ui,
    colors: &ThemeColors,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> egui::InnerResponse<R> {
    // `Frame::show` wraps the closure's own `InnerResponse` (from
    // `ui.vertical`); unwrap both levels so callers see the frame.
    let outer = content_frame(colors).show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        ui.vertical(add_contents)
    });
    egui::InnerResponse::new(outer.inner.inner, outer.response)
}

/// Nested card/container frame (device box, routing box, card list).
pub fn nested_frame(colors: &ThemeColors, margin: impl Into<egui::Margin>) -> egui::Frame {
    egui::Frame::new()
        .fill(colors.deep_bg)
        .corner_radius(10.0)
        .stroke(Stroke::new(1.0, colors.border_main))
        .inner_margin(margin)
}

fn galley_for(
    ui: &Ui,
    label: &str,
    font: FontSpec,
    color: Color32,
) -> std::sync::Arc<egui::Galley> {
    egui::WidgetText::from(font.with(label).color(color)).into_galley(
        ui,
        Some(TextWrapMode::Extend),
        f32::INFINITY,
        egui::FontSelection::Default,
    )
}

fn paint_surface(
    ui: &Ui,
    rect: egui::Rect,
    radius: f32,
    fill: Color32,
    stroke: Stroke,
    galley: &std::sync::Arc<egui::Galley>,
) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, radius, fill);
    if stroke.width > 0.0 {
        painter.rect_stroke(rect, radius, stroke, egui::StrokeKind::Inside);
    }
    painter.galley(
        rect.center() - galley.size() / 2.0,
        galley.clone(),
        Color32::PLACEHOLDER,
    );
}

/// Map (selected, enabled, hovered) to the option interaction state.
/// Selected and disabled elements are not hover-styled: re-clicking a
/// selected option is a no-op, so it must not advertise clickability.
fn option_state(selected: bool, enabled: bool, hovered: bool) -> OptionState {
    if !enabled {
        OptionState::Disabled
    } else if selected {
        OptionState::Selected
    } else if hovered {
        OptionState::Hover
    } else {
        OptionState::Idle
    }
}

/// Solid-fill action button (Primary / Danger / Success / Muted).
/// Custom-painted so hover/press feedback survives the explicit fill.
pub fn action_button(
    ui: &mut Ui,
    colors: &ThemeColors,
    style: ActionStyle,
    label: &str,
    font: FontSpec,
    min_size: Vec2,
    enabled: bool,
) -> egui::Response {
    let text = filled_action_visuals(colors, style, false, false, enabled).text;
    let galley = galley_for(ui, label, font.bold(), text);
    let pad = ui.style().spacing.button_padding;
    let desired = Vec2::new(
        (galley.size().x + 2.0 * pad.x).max(min_size.x),
        (galley.size().y + 2.0 * pad.y).max(min_size.y),
    );
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(desired, sense);
    let resp = if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    };
    let v = filled_action_visuals(
        colors,
        style,
        resp.hovered() && enabled,
        resp.is_pointer_button_down_on() && enabled,
        enabled,
    );
    paint_surface(ui, rect, 8.0, v.fill, Stroke::NONE, &galley);
    resp
}

/// One-of-many option chip (sample rate, channel, FPS selector).
pub fn option_chip(
    ui: &mut Ui,
    colors: &ThemeColors,
    label: &str,
    font: FontSpec,
    selected: bool,
    enabled: bool,
    min_size: Vec2,
) -> egui::Response {
    let state = option_state(selected, enabled, false);
    let v = option_visuals(colors, state);
    let font = if v.strong { font.bold() } else { font };
    let galley = galley_for(ui, label, font, v.text);
    let pad = ui.style().spacing.button_padding;
    let desired = Vec2::new(
        (galley.size().x + 2.0 * pad.x).max(min_size.x),
        (galley.size().y + 2.0 * pad.y).max(min_size.y),
    );
    let sense = if state == OptionState::Idle {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(desired, sense);
    let resp = if state == OptionState::Idle {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    };
    // Re-decode for hover (only idle elements are hoverable).
    let v = if state == OptionState::Idle && resp.hovered() {
        option_visuals(colors, OptionState::Hover)
    } else {
        v
    };
    paint_surface(ui, rect, 8.0, v.fill, v.stroke, &galley);
    resp
}

/// One-of-many option card with title + description (FPS selector).
pub fn option_card(
    ui: &mut Ui,
    colors: &ThemeColors,
    title: &str,
    subtitle: &str,
    selected: bool,
    enabled: bool,
) -> egui::Response {
    let state = option_state(selected, enabled, false);
    let v = option_visuals(colors, state);
    let title_galley = galley_for(ui, title, crate::text::font(13.0).mono().bold(), v.text);
    let subtitle_galley = galley_for(
        ui,
        subtitle,
        TextStyle::Description.font_spec(),
        colors.text_muted,
    );
    let width = ui.available_width();
    let height = 60.0_f32.max(title_galley.size().y + subtitle_galley.size().y + 20.0);
    let sense = if state == OptionState::Idle {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, height), sense);
    let resp = if state == OptionState::Idle {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    };
    let v = if state == OptionState::Idle && resp.hovered() {
        option_visuals(colors, OptionState::Hover)
    } else {
        v
    };
    // Subtitle is muted in every state; repaint it over the frame.
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 8.0, v.fill);
    painter.rect_stroke(rect, 8.0, v.stroke, egui::StrokeKind::Inside);
    let inner = rect.shrink2(Vec2::new(10.0, 10.0));
    painter.galley(inner.left_top(), title_galley.clone(), Color32::PLACEHOLDER);
    painter.galley(
        egui::pos2(inner.left(), inner.top() + title_galley.size().y + 2.0),
        subtitle_galley,
        Color32::PLACEHOLDER,
    );
    resp
}

/// Static, never-clickable semantic badge.
pub fn static_badge(
    ui: &mut Ui,
    colors: &ThemeColors,
    tone: BadgeTone,
    text: &str,
    font: FontSpec,
) -> egui::Response {
    let (fill, stroke, text_color) = badge_colors(colors, tone);
    let galley = galley_for(ui, text, font, text_color);
    let margin = egui::Margin::symmetric(8, 4);
    let desired = Vec2::new(
        galley.size().x + f32::from(margin.left) + f32::from(margin.right),
        galley.size().y + f32::from(margin.top) + f32::from(margin.bottom),
    );
    let (rect, resp) = ui.allocate_exact_size(desired, Sense::hover());
    paint_surface(ui, rect, 4.0, fill, stroke, &galley);
    resp
}

/// Shared step header: static accent-numbered badge + strong label.
/// Replaces the per-tab copies (converter solid-accent square,
/// offload unstyled label).
pub fn step_header(ui: &mut Ui, colors: &ThemeColors, number: &str, label: &str) {
    ui.horizontal(|ui| {
        static_badge(
            ui,
            colors,
            BadgeTone::Accent,
            number,
            crate::text::font(11.0).mono(),
        );
        ui.label(
            text(ui, label)
                .style(TextStyle::TabHeading, colors)
                .color(colors.text_title),
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    fn colors() -> ThemeColors {
        Theme::Dark.colors()
    }

    #[test]
    fn selected_option_is_never_styled_like_primary_action() {
        let c = colors();
        let sel = option_visuals(&c, OptionState::Selected);
        let primary = filled_action_visuals(&c, ActionStyle::Primary, false, false, true);
        assert_ne!(
            sel.fill, primary.fill,
            "selected fill must be a tint, not solid accent"
        );
        assert_ne!(
            sel.text, primary.text,
            "selected text must be accent, not black"
        );
        assert!(
            sel.stroke.width > 0.0,
            "selected keeps an accent stroke; primary has none"
        );
    }

    #[test]
    fn option_hover_distinct_from_idle_and_selected() {
        let c = colors();
        let idle = option_visuals(&c, OptionState::Idle);
        let hover = option_visuals(&c, OptionState::Hover);
        let sel = option_visuals(&c, OptionState::Selected);
        assert_ne!(hover.fill, idle.fill);
        assert_ne!(hover.stroke, idle.stroke);
        assert_ne!(hover.fill, sel.fill);
        assert_ne!(hover.stroke, sel.stroke);
        assert_ne!(hover.strong, sel.strong);
    }

    #[test]
    fn option_hover_signals_affordance_with_accent_stroke() {
        let c = colors();
        let hover = option_visuals(&c, OptionState::Hover);
        assert!(hover.stroke.width > 1.0);
        assert_ne!(hover.stroke.color, colors().border_main);
    }

    #[test]
    fn disabled_option_is_dimmed() {
        let c = colors();
        let dis = option_visuals(&c, OptionState::Disabled);
        assert_eq!(dis.text, c.text_muted);
    }

    #[test]
    fn solid_actions_lighten_on_hover_and_darken_when_pressed() {
        let c = colors();
        for style in [
            ActionStyle::Primary,
            ActionStyle::Danger,
            ActionStyle::Success,
        ] {
            let base = filled_action_visuals(&c, style, false, false, true);
            let hover = filled_action_visuals(&c, style, true, false, true);
            let press = filled_action_visuals(&c, style, false, true, true);
            assert_ne!(hover.fill, base.fill, "{style:?} hover must differ");
            assert_ne!(press.fill, base.fill, "{style:?} press must differ");
            assert_ne!(hover.fill, press.fill);
        }
    }

    #[test]
    fn muted_action_hover_uses_nested_hover() {
        let c = colors();
        let hover = filled_action_visuals(&c, ActionStyle::Muted, true, false, true);
        assert_eq!(hover.fill, c.nested_hover);
    }

    #[test]
    fn disabled_action_is_dimmed() {
        let c = colors();
        let dis = filled_action_visuals(&c, ActionStyle::Primary, false, false, false);
        assert_eq!(dis.text, c.text_muted);
        assert_ne!(dis.fill, ACCENT);
    }

    #[test]
    fn matrix_selected_distinct_from_hover_distinct_from_idle() {
        let c = colors();
        let idle = matrix_cell_visuals(&c, false, false, false);
        let hover = matrix_cell_visuals(&c, false, true, false);
        let sel = matrix_cell_visuals(&c, true, false, false);
        assert_ne!(hover.ring, idle.ring);
        assert_ne!(hover.core, idle.core);
        assert_ne!(sel.ring, hover.ring);
        assert_ne!(sel.core, hover.core);
    }

    #[test]
    fn step_badge_is_static_not_primary() {
        let c = colors();
        let (fill, stroke, _) = badge_colors(&c, BadgeTone::Accent);
        let primary = filled_action_visuals(&c, ActionStyle::Primary, false, false, true);
        assert_ne!(
            fill, primary.fill,
            "static badge must not be a solid accent square"
        );
        assert!(stroke.width > 0.0);
    }

    #[test]
    fn content_frame_is_the_shared_panel_contract() {
        let c = colors();
        let f = content_frame(&c);
        assert_eq!(f.fill, c.card_bg);
        assert_eq!(f.stroke.width, 1.5);
        assert_eq!(f.corner_radius, egui::CornerRadius::same(12));
    }

    /// Headless egui pass — `Context::run_ui` needs no display backend.
    fn run_headless_ui(width: f32, body: impl FnMut(&mut Ui)) {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, 400.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run_ui(input, body);
    }

    #[test]
    fn content_area_spans_the_full_available_width() {
        let c = colors();
        let panel_w = 600.0;
        let mut frame_w = 0.0;
        run_headless_ui(panel_w, |ui| {
            frame_w = content_area(ui, &c, |ui| {
                ui.label("tiny");
            })
            .response
            .rect
            .width();
        });
        // The frame rect (including its margins) must be exactly the deck
        // row the header/footer span — a tiny label must not shrink it.
        assert!(
            (frame_w - panel_w).abs() < 0.5,
            "content_area must fill the row (frame width {frame_w}, row {panel_w})"
        );
    }

    #[test]
    fn bare_content_frame_shrink_wraps_to_its_content() {
        let c = colors();
        let panel_w = 600.0;
        let mut frame_w = 0.0;
        run_headless_ui(panel_w, |ui| {
            frame_w = content_frame(&c)
                .show(ui, |ui| {
                    ui.label("tiny");
                })
                .response
                .rect
                .width();
        });
        assert!(
            frame_w < panel_w - 100.0,
            "a bare frame shrink-wraps to its widest child (got {frame_w}); \
             the min-width pin in content_area is what widens the tab cards"
        );
    }

    #[test]
    fn static_badge_tones_are_tints_not_solids() {
        let c = colors();
        for tone in [BadgeTone::Warning, BadgeTone::Danger, BadgeTone::Accent] {
            let (fill, _, _) = badge_colors(&c, tone);
            let solid = match tone {
                BadgeTone::Warning => c.warning_amber,
                BadgeTone::Danger => c.error_red,
                BadgeTone::Accent => ACCENT,
            };
            assert_ne!(fill, solid, "{tone:?} badge fill must be a tint");
        }
    }
}
