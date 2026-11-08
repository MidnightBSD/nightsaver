//! CPU framebuffer and drawing helpers. Owner: Sonnet D.
pub mod font;
pub mod oled;

/// 0x00RRGGBB pixels, row-major, top-left origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Framebuffer {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u32>,
}

impl Framebuffer {
    pub fn new(width: u32, height: u32) -> Self {
        Framebuffer { width, height, pixels: vec![0; (width as usize) * (height as usize)] }
    }
    pub fn clear(&mut self) {
        self.pixels.fill(0);
    }
    #[inline]
    pub fn put(&mut self, x: i32, y: i32, rgb: u32) {
        if x >= 0 && y >= 0 && (x as u32) < self.width && (y as u32) < self.height {
            self.pixels[(y as u32 * self.width + x as u32) as usize] = rgb;
        }
    }
    #[inline]
    pub fn get(&self, x: i32, y: i32) -> Option<u32> {
        if x >= 0 && y >= 0 && (x as u32) < self.width && (y as u32) < self.height {
            Some(self.pixels[(y as u32 * self.width + x as u32) as usize])
        } else {
            None
        }
    }
    pub fn fill_rect(&mut self, x: i32, y: i32, w: u32, h: u32, rgb: u32) {
        for yy in y..y + h as i32 {
            for xx in x..x + w as i32 {
                self.put(xx, yy, rgb);
            }
        }
    }
    /// Fraction of non-black pixels.
    pub fn lit_fraction(&self) -> f32 {
        let lit = self.pixels.iter().filter(|&&p| p & 0x00ff_ffff != 0).count();
        lit as f32 / self.pixels.len().max(1) as f32
    }
}

#[inline]
pub fn rgb(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

/// Scale a colour by `f` in 0.0..=1.0.
#[inline]
pub fn scale(rgb: u32, f: f32) -> u32 {
    let f = f.clamp(0.0, 1.0);
    let r = (((rgb >> 16) & 0xff) as f32 * f) as u32;
    let g = (((rgb >> 8) & 0xff) as f32 * f) as u32;
    let b = ((rgb & 0xff) as f32 * f) as u32;
    (r << 16) | (g << 8) | b
}
