pub mod clock;
pub mod clapper;
pub mod settings;
pub mod status;

use egui::{Color32, Ui};

/// Draw a small colored pill/badge with text.
pub fn pill(ui: &mut Ui, text: &str, color: Color32) {
    let frame = egui::Frame::new()
        .corner_radius(4.0)
        .fill(color.linear_multiply(0.2))
        .stroke(egui::Stroke::new(1.0, color))
        .inner_margin(egui::Margin::symmetric(8, 2));
    frame.show(ui, |ui| {
        ui.label(egui::RichText::new(text).color(color).small());
    });
}
