use std::fs;
use std::path::Path;
use std::sync::Arc;

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Shadow,
    Stroke, Style, TextStyle, Theme, ThemePreference, Visuals,
};

pub const SPACE_XS: f32 = 4.0;
pub const SPACE_S: f32 = 8.0;
pub const SPACE_M: f32 = 12.0;
pub const SPACE_L: f32 = 16.0;
pub const SPACE_XL: f32 = 24.0;
pub const CARD_RADIUS: u8 = 8;
pub const CONTROL_RADIUS: u8 = 6;

pub const CAPTION: f32 = 12.0;
pub const BODY: f32 = 14.0;
pub const SECTION: f32 = 16.0;
pub const PAGE: f32 = 22.0;
pub const DISPLAY: f32 = 28.0;

const FONTS_DIR: &str = r"C:\Windows\Fonts";
const SEMIBOLD: &str = "semibold";
const ICONS: &str = "icons";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Accent,
    Ok,
    Warn,
    Danger,
}

#[derive(Debug)]
pub struct Palette {
    pub bg: Color32,
    pub surface: Color32,
    pub surface_raised: Color32,
    pub control: Color32,
    pub control_hover: Color32,
    pub segment: Color32,
    pub border: Color32,
    pub text: Color32,
    pub text_muted: Color32,
    pub accent: Color32,
    pub on_accent: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub danger: Color32,
    fill_alpha: f32,
}

pub const DARK: Palette = Palette {
    bg: Color32::from_rgb(24, 25, 29),
    surface: Color32::from_rgb(33, 35, 40),
    surface_raised: Color32::from_rgb(42, 44, 51),
    control: Color32::from_rgb(48, 51, 59),
    control_hover: Color32::from_rgb(58, 61, 70),
    segment: Color32::from_rgb(72, 76, 87),
    border: Color32::from_rgb(54, 57, 65),
    text: Color32::from_rgb(234, 235, 239),
    text_muted: Color32::from_rgb(156, 160, 171),
    accent: Color32::from_rgb(86, 160, 255),
    on_accent: Color32::from_rgb(10, 20, 36),
    ok: Color32::from_rgb(82, 199, 140),
    warn: Color32::from_rgb(232, 184, 72),
    danger: Color32::from_rgb(242, 108, 96),
    fill_alpha: 0.16,
};

pub const LIGHT: Palette = Palette {
    bg: Color32::from_rgb(243, 244, 247),
    surface: Color32::from_rgb(255, 255, 255),
    surface_raised: Color32::from_rgb(248, 249, 251),
    control: Color32::from_rgb(236, 238, 242),
    control_hover: Color32::from_rgb(226, 229, 235),
    segment: Color32::from_rgb(255, 255, 255),
    border: Color32::from_rgb(222, 225, 231),
    text: Color32::from_rgb(26, 28, 34),
    text_muted: Color32::from_rgb(92, 97, 110),
    accent: Color32::from_rgb(20, 104, 214),
    on_accent: Color32::from_rgb(255, 255, 255),
    ok: Color32::from_rgb(22, 128, 78),
    warn: Color32::from_rgb(158, 98, 0),
    danger: Color32::from_rgb(196, 50, 40),
    fill_alpha: 0.12,
};

impl Palette {
    pub fn tone(&self, tone: Tone) -> Color32 {
        match tone {
            Tone::Neutral => self.text_muted,
            Tone::Accent => self.accent,
            Tone::Ok => self.ok,
            Tone::Warn => self.warn,
            Tone::Danger => self.danger,
        }
    }

    pub fn fill(&self, tone: Tone) -> Color32 {
        self.tone(tone).gamma_multiply(self.fill_alpha)
    }
}

pub fn of(ui: &egui::Ui) -> &'static Palette {
    for_dark(ui.visuals().dark_mode)
}

pub fn for_dark(dark_mode: bool) -> &'static Palette {
    if dark_mode { &DARK } else { &LIGHT }
}

pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SEMIBOLD.into()))
}

pub fn icon_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(ICONS.into()))
}

pub fn install(ctx: &egui::Context) {
    ctx.set_fonts(fonts(Path::new(FONTS_DIR)));
    ctx.set_style_of(Theme::Dark, style(Visuals::dark(), &DARK));
    ctx.set_style_of(Theme::Light, style(Visuals::light(), &LIGHT));
    ctx.set_theme(ThemePreference::System);
}

fn load(dir: &Path, file: &str) -> Option<FontData> {
    fs::read(dir.join(file)).ok().map(FontData::from_owned)
}

fn fonts(dir: &Path) -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let fallbacks = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let mut add = |name: &str, files: &[&str]| {
        let data = files.iter().find_map(|file| load(dir, file))?;
        fonts.font_data.insert(name.to_owned(), Arc::new(data));
        Some(name.to_owned())
    };
    let regular = add("segoe-ui", &["segoeui.ttf"]);
    let semibold = add("segoe-ui-semibold", &["seguisb.ttf", "segoeuib.ttf"]);
    let icons = add("segoe-icons", &["SegoeIcons.ttf", "segmdl2.ttf"]);

    let chain = |first: Option<String>| -> Vec<String> {
        first
            .into_iter()
            .chain(fallbacks.iter().cloned())
            .chain(icons.clone())
            .collect()
    };
    let proportional = chain(regular.clone());
    let semibold = match semibold {
        Some(name) => chain(Some(name)),
        None => proportional.clone(),
    };
    let icon_chain = icons
        .iter()
        .cloned()
        .chain(fallbacks.iter().cloned())
        .collect();
    fonts
        .families
        .insert(FontFamily::Proportional, proportional);
    fonts
        .families
        .insert(FontFamily::Name(SEMIBOLD.into()), semibold);
    fonts
        .families
        .insert(FontFamily::Name(ICONS.into()), icon_chain);
    fonts
}

fn widget(fill: Color32, stroke: Color32, text: Color32, radius: u8) -> egui::style::WidgetVisuals {
    egui::style::WidgetVisuals {
        bg_fill: fill,
        weak_bg_fill: fill,
        bg_stroke: Stroke::new(1.0, stroke),
        corner_radius: CornerRadius::same(radius),
        fg_stroke: Stroke::new(1.0, text),
        expansion: 0.0,
    }
}

fn style(mut visuals: Visuals, palette: &Palette) -> Style {
    let shadow = Shadow {
        offset: [0, 4],
        blur: 16,
        spread: 0,
        color: Color32::from_black_alpha(if visuals.dark_mode { 96 } else { 28 }),
    };
    visuals.panel_fill = palette.bg;
    visuals.window_fill = palette.surface;
    visuals.window_stroke = Stroke::new(1.0, palette.border);
    visuals.window_corner_radius = CornerRadius::same(CARD_RADIUS + 2);
    visuals.window_shadow = shadow;
    visuals.popup_shadow = shadow;
    visuals.menu_corner_radius = CornerRadius::same(CARD_RADIUS);
    visuals.extreme_bg_color = palette.surface_raised;
    visuals.faint_bg_color = palette.surface_raised;
    visuals.text_edit_bg_color = Some(palette.surface_raised);
    visuals.code_bg_color = palette.control;
    visuals.hyperlink_color = palette.accent;
    visuals.warn_fg_color = palette.warn;
    visuals.error_fg_color = palette.danger;
    visuals.selection.bg_fill = palette.fill(Tone::Accent).gamma_multiply(1.6);
    visuals.selection.stroke = Stroke::new(1.0, palette.accent);
    visuals.slider_trailing_fill = true;
    visuals.collapsing_header_frame = false;
    visuals.indent_has_left_vline = false;
    visuals.striped = false;
    visuals.widgets.noninteractive = widget(
        palette.surface,
        palette.border,
        palette.text,
        CONTROL_RADIUS,
    );
    visuals.widgets.inactive = widget(
        palette.control,
        Color32::TRANSPARENT,
        palette.text,
        CONTROL_RADIUS,
    );
    visuals.widgets.hovered = widget(
        palette.control_hover,
        palette.border,
        palette.text,
        CONTROL_RADIUS,
    );
    visuals.widgets.active = widget(
        palette.control_hover,
        palette.accent,
        palette.text,
        CONTROL_RADIUS,
    );
    visuals.widgets.open = widget(
        palette.control_hover,
        palette.border,
        palette.text,
        CONTROL_RADIUS,
    );

    let mut style = Style {
        visuals,
        ..Style::default()
    };
    style.text_styles = [
        (TextStyle::Small, FontId::proportional(CAPTION)),
        (TextStyle::Body, FontId::proportional(BODY)),
        (TextStyle::Button, FontId::proportional(BODY)),
        (TextStyle::Monospace, FontId::monospace(13.0)),
        (TextStyle::Heading, semibold(PAGE)),
    ]
    .into();
    let spacing = &mut style.spacing;
    spacing.item_spacing = egui::vec2(SPACE_S, SPACE_S);
    spacing.button_padding = egui::vec2(SPACE_M, 5.0);
    spacing.interact_size = egui::vec2(40.0, 28.0);
    spacing.menu_margin = Margin::same(6);
    spacing.window_margin = Margin::same(SPACE_L as i8);
    spacing.slider_rail_height = 4.0;
    spacing.slider_width = 220.0;
    spacing.combo_width = 180.0;
    spacing.icon_width = 16.0;
    spacing.icon_width_inner = 8.0;
    spacing.icon_spacing = 6.0;
    style
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(color: Color32) -> f32 {
        let channel = |value: u8| {
            let value = f32::from(value) / 255.0;
            if value <= 0.039_28 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
    }

    fn contrast(a: Color32, b: Color32) -> f32 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn text_and_status_colours_meet_wcag_aa_on_every_surface() {
        for palette in [&DARK, &LIGHT] {
            for surface in [palette.bg, palette.surface, palette.surface_raised] {
                assert!(contrast(palette.text, surface) >= 7.0);
                for color in [
                    palette.text_muted,
                    palette.accent,
                    palette.ok,
                    palette.warn,
                    palette.danger,
                ] {
                    assert!(
                        contrast(color, surface) >= 4.5,
                        "{color:?} on {surface:?} is {:.2}",
                        contrast(color, surface)
                    );
                }
            }
        }
    }

    #[test]
    fn missing_system_fonts_fall_back_to_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let fonts = fonts(dir.path());
        let defaults = FontDefinitions::default();
        assert_eq!(
            fonts.families[&FontFamily::Proportional],
            defaults.families[&FontFamily::Proportional]
        );
        assert!(!fonts.families[&FontFamily::Name(SEMIBOLD.into())].is_empty());
    }
}
