#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Color32(pub u8, pub u8, pub u8);

impl Color32 {
    pub const fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Self(r, g, b)
    }

    pub const fn from_gray(gray: u8) -> Self {
        Self(gray, gray, gray)
    }

    pub fn mix(self, other: Self, weight: f64) -> Self {
        let blend =
            |a: u8, b: u8| (f64::from(a) * (1.0 - weight) + f64::from(b) * weight).round() as u8;
        Self(
            blend(self.0, other.0),
            blend(self.1, other.1),
            blend(self.2, other.2),
        )
    }

    pub fn cairo(self, ctx: &gtk::cairo::Context) {
        ctx.set_source_rgb(
            f64::from(self.0) / 255.0,
            f64::from(self.1) / 255.0,
            f64::from(self.2) / 255.0,
        );
    }
}

pub const PROMPT_BADGE: Color32 = Color32(3, 7, 11);
pub const PROMPT_DIRECTORY: Color32 = Color32(5, 9, 13);
pub const PROMPT_DIRECTORY_ALT: Color32 = Color32(7, 11, 17);
pub const PROMPT_JOIN: Color32 = Color32(9, 13, 19);
