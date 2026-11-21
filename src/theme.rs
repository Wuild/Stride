use gtk::{gdk, prelude::*};

use crate::color::Color32;

#[derive(Clone, Copy)]
pub struct Theme {
    pub bg: Color32,
    pub fg: Color32,
    pub muted: Color32,
    pub surface: Color32,
    pub accent: Color32,
    pub accent_fg: Color32,
    pub selection: Color32,
    pub dark: bool,
}

impl Theme {
    pub fn from_widget(widget: &gtk::Widget) -> Self {
        let style = widget.style_context();
        let manager = adw::StyleManager::default();
        let dark = manager.is_dark();
        let get = |name, fallback: Color32| {
            style
                .lookup_color(name)
                .map(|value| fallback.mix(rgb(value), f64::from(value.alpha())))
                .unwrap_or(fallback)
        };
        let bg = get(
            "view_bg_color",
            Color32::from_gray(if dark { 29 } else { 255 }),
        );
        let fg = get(
            "view_fg_color",
            Color32::from_gray(if dark { 242 } else { 36 }),
        );
        let accent = get("accent_bg_color", Color32(53, 132, 228));
        let accent_fg = get("accent_fg_color", Color32(255, 255, 255));
        Self {
            bg,
            fg,
            accent,
            accent_fg,
            dark,
            muted: bg.mix(fg, 0.58),
            surface: get("card_bg_color", bg),
            selection: bg.mix(accent, 0.56),
        }
    }
}

fn rgb(color: gdk::RGBA) -> Color32 {
    let byte = |v: f32| (255.0 * v.clamp(0.0, 1.0)).round() as u8;
    Color32(byte(color.red()), byte(color.green()), byte(color.blue()))
}
