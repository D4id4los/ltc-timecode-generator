#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    #[inline]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Rgb(r, g, b)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThemeColors {
    pub accent: Rgb,
    pub app_bg: Rgb,
    pub card_bg: Rgb,
    pub deep_bg: Rgb,
    pub nested_bg: Rgb,
    pub nested_hover: Rgb,
    pub text_main: Rgb,
    pub text_title: Rgb,
    pub text_muted: Rgb,
    pub text_secondary: Rgb,
    pub border_main: Rgb,
    pub btn_bg: Rgb,
    pub clock_sep: Rgb,
    pub error_red: Rgb,
    pub warning_amber: Rgb,
    pub success_green: Rgb,
    pub info_blue: Rgb,
}

pub const DARK: ThemeColors = ThemeColors {
    accent: Rgb(0xFF, 0x5F, 0x1F),
    app_bg: Rgb(0x0A, 0x0A, 0x0B),
    card_bg: Rgb(0x1A, 0x1A, 0x1E),
    deep_bg: Rgb(0x0A, 0x0A, 0x0B),
    nested_bg: Rgb(0x14, 0x14, 0x16),
    nested_hover: Rgb(0x1C, 0x1C, 0x20),
    text_main: Rgb(0xE0, 0xE0, 0xE0),
    text_title: Rgb(0xFF, 0xFF, 0xFF),
    text_muted: Rgb(0x8E, 0x92, 0x99),
    text_secondary: Rgb(0xCC, 0xCC, 0xCC),
    border_main: Rgb(0x2A, 0x2A, 0x2E),
    btn_bg: Rgb(0x1A, 0x1A, 0x1E),
    clock_sep: Rgb(0x3F, 0x3F, 0x46),
    error_red: Rgb(0xEF, 0x44, 0x44),
    warning_amber: Rgb(0xF5, 0x9E, 0x0B),
    success_green: Rgb(0x22, 0xC5, 0x5E),
    info_blue: Rgb(0x3B, 0x82, 0xF6),
};

pub const LIGHT: ThemeColors = ThemeColors {
    accent: Rgb(0xFF, 0x5F, 0x1F),
    app_bg: Rgb(0xF4, 0xF4, 0xF6),
    card_bg: Rgb(0xFF, 0xFF, 0xFF),
    deep_bg: Rgb(0xEB, 0xEB, 0xEF),
    nested_bg: Rgb(0xF4, 0xF4, 0xF6),
    nested_hover: Rgb(0xE2, 0xE2, 0xE7),
    text_main: Rgb(0x27, 0x27, 0x2A),
    text_title: Rgb(0x09, 0x09, 0x0B),
    text_muted: Rgb(0x71, 0x71, 0x7A),
    text_secondary: Rgb(0x3F, 0x3F, 0x46),
    border_main: Rgb(0xE4, 0xE4, 0xE7),
    btn_bg: Rgb(0xFF, 0xFF, 0xFF),
    clock_sep: Rgb(0xD4, 0xD4, 0xD8),
    error_red: Rgb(0xDC, 0x26, 0x26),
    warning_amber: Rgb(0xD9, 0x77, 0x06),
    success_green: Rgb(0x16, 0xA3, 0x4A),
    info_blue: Rgb(0x25, 0x63, 0xEB),
};

pub fn palette(is_dark: bool) -> &'static ThemeColors {
    if is_dark { &DARK } else { &LIGHT }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_palette_dark() {
        let p = palette(true);
        assert_eq!(p.app_bg, DARK.app_bg);
        assert_eq!(p.text_main, DARK.text_main);
        assert_eq!(p.accent, DARK.accent);
    }

    #[test]
    fn test_palette_light() {
        let p = palette(false);
        assert_eq!(p.app_bg, LIGHT.app_bg);
        assert_eq!(p.text_main, LIGHT.text_main);
        assert_eq!(p.accent, LIGHT.accent);
    }

    #[test]
    fn test_palette_dark_not_light() {
        let dark = palette(true);
        let light = palette(false);
        assert_ne!(dark.app_bg, light.app_bg);
        assert_ne!(dark.text_main, light.text_main);
    }

    #[test]
    fn test_accent_same_in_both() {
        assert_eq!(DARK.accent, LIGHT.accent);
    }

    #[test]
    fn test_theme_colors_are_different() {
        assert_ne!(DARK.app_bg, DARK.card_bg);
        assert_ne!(DARK.text_main, DARK.text_muted);
    }

    #[test]
    fn test_rgb_new() {
        let c = Rgb::new(0xFF, 0x5F, 0x1F);
        assert_eq!(c.0, 0xFF);
        assert_eq!(c.1, 0x5F);
        assert_eq!(c.2, 0x1F);
    }

    #[test]
    fn test_rgb_debug() {
        let c = Rgb(255, 95, 31);
        let d = format!("{:?}", c);
        assert!(d.contains("255"));
        assert!(d.contains("95"));
    }
}