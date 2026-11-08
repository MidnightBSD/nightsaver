//! MidnightBSD logo pattern: the crescent-and-cat logo drawn as a drifting outline with
//! sparse twinkling "stardust" inside the silhouette, and the boot loader's ASCII brand
//! beneath it. Only edges and a few scattered pixels are lit, so it stays OLED friendly.
use super::{Pattern, Rng};
use crate::render::font::{self, GLYPH_H};
use crate::render::{Framebuffer, rgb, scale};
use std::f32::consts::TAU;
use std::time::Duration;

/// 1-bit mask of the logo (PBM P4, see `assets/README.md`).
const LOGO_PBM: &[u8] = include_bytes!("assets/midnightbsd-logo.pbm");

/// The brand as drawn by the MidnightBSD boot loader (`stand/lua/drawer.lua`).
const BRAND: [&str; 7] = [
    "                       ____   _____ _____  ",
    "                      |  _ \\ / ____|  __ \\ ",
    "                      | |_) | (___ | |  | |",
    "      M i d n i g h t |  _ < \\___ \\| |  | |",
    "                      | |_) |____) | |__| |",
    "                      |     |      |      |",
    "                      |____/|_____/|_____/ ",
];

/// Drift periods in seconds for the Lissajous path (x, y).
const PERIOD_X: f32 = 540.0;
const PERIOD_Y: f32 = 347.0;
/// One in this many silhouette pixels becomes a dust point.
const DUST_SPACING: u32 = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Mask {
    w: usize,
    h: usize,
}

/// Parse the PBM header; returns the dimensions and the packed rows.
fn parse_pbm(data: &[u8]) -> Option<(Mask, &[u8])> {
    let text = std::str::from_utf8(data.get(..32)?).ok().or_else(|| {
        // Header is ASCII; stop at the first non-UTF-8 byte.
        let end = data.iter().take(32).position(|b| *b >= 0x80).unwrap_or(32);
        std::str::from_utf8(&data[..end]).ok()
    })?;
    let mut it = text.split_ascii_whitespace();
    if it.next()? != "P4" {
        return None;
    }
    let w: usize = it.next()?.parse().ok()?;
    let h: usize = it.next()?.parse().ok()?;
    // Body starts after the single whitespace byte that follows the height.
    let hdr_end = {
        let mut seen = 0;
        let mut i = 0;
        while i < data.len() && seen < 3 {
            while i < data.len() && !data[i].is_ascii_whitespace() {
                i += 1;
            }
            seen += 1;
            i += 1;
        }
        i
    };
    let body = data.get(hdr_end..hdr_end + h * w.div_ceil(8))?;
    Some((Mask { w, h }, body))
}

fn mask_bit(m: Mask, body: &[u8], x: usize, y: usize) -> bool {
    if x >= m.w || y >= m.h {
        return false;
    }
    let stride = m.w.div_ceil(8);
    body[y * stride + x / 8] >> (7 - (x % 8)) & 1 == 1
}

#[derive(Debug, Default)]
pub struct Midnight {
    width: u32,
    height: u32,
    /// Scaled silhouette (row-major, `lw` x `lh`).
    silhouette: Vec<bool>,
    lw: usize,
    lh: usize,
    /// Edge pixels of the scaled silhouette, relative to the logo origin.
    outline: Vec<(u16, u16)>,
    /// Scattered points inside the silhouette with a twinkle phase.
    dust: Vec<(u16, u16, f32)>,
    text_scale: u32,
    t: f32,
    phase: (f32, f32),
}

impl Midnight {
    fn build(&mut self, rng: &mut Rng) {
        self.silhouette.clear();
        self.outline.clear();
        self.dust.clear();
        let Some((m, body)) = parse_pbm(LOGO_PBM) else {
            self.lw = 0;
            self.lh = 0;
            return;
        };
        // Logo takes about a third of the width or 40% of the height, whichever is smaller.
        let target_w = (self.width as f32 / 3.0).min(self.height as f32 * 0.4 * m.w as f32 / m.h as f32);
        let factor = (target_w / m.w as f32).max(0.05);
        self.lw = ((m.w as f32 * factor) as usize).max(1);
        self.lh = ((m.h as f32 * factor) as usize).max(1);
        self.silhouette = (0..self.lw * self.lh)
            .map(|i| {
                let (x, y) = (i % self.lw, i / self.lw);
                mask_bit(m, body, (x as f32 / factor) as usize, (y as f32 / factor) as usize)
            })
            .collect();
        let at = |x: isize, y: isize| -> bool {
            x >= 0 && y >= 0 && (x as usize) < self.lw && (y as usize) < self.lh && self.silhouette[y as usize * self.lw + x as usize]
        };
        for y in 0..self.lh {
            for x in 0..self.lw {
                if !self.silhouette[y * self.lw + x] {
                    continue;
                }
                let (xi, yi) = (x as isize, y as isize);
                let edge = !(at(xi - 1, yi) && at(xi + 1, yi) && at(xi, yi - 1) && at(xi, yi + 1));
                if edge {
                    self.outline.push((x as u16, y as u16));
                } else if rng.next_u32().is_multiple_of(DUST_SPACING) {
                    self.dust.push((x as u16, y as u16, rng.next_f32() * TAU));
                }
            }
        }
        // Brand text roughly as wide as the logo.
        let line_w = font::text_width(BRAND[0], 1).max(1) as u32;
        self.text_scale = ((self.lw as u32 * 2 / line_w).div_ceil(2)).max(1);
    }

    fn total_size(&self) -> (i32, i32) {
        let tw = font::text_width(BRAND[0], self.text_scale);
        let th = (BRAND.len() as u32 * (GLYPH_H + 1) * self.text_scale) as i32;
        let gap = (self.lh / 8) as i32;
        ((self.lw as i32).max(tw), self.lh as i32 + gap + th)
    }
}

impl Pattern for Midnight {
    fn name(&self) -> &'static str {
        "midnight"
    }
    fn init(&mut self, width: u32, height: u32, _lit_budget: f32, rng: &mut Rng) {
        self.width = width;
        self.height = height;
        self.t = 0.0;
        self.phase = (rng.next_f32() * TAU, rng.next_f32() * TAU);
        self.build(rng);
    }
    fn step(&mut self, dt: Duration, fb: &mut Framebuffer, _rng: &mut Rng) {
        self.t += dt.as_secs_f32().min(0.5);
        if self.lw == 0 {
            return;
        }
        let (cw, ch) = self.total_size();
        let (w, h) = (fb.width as i32, fb.height as i32);
        let ax = ((w - cw).max(0) as f32) / 2.0;
        let ay = ((h - ch).max(0) as f32) / 2.0;
        let ox = (ax + ax * (self.t / PERIOD_X * TAU + self.phase.0).sin()) as i32;
        let oy = (ay + ay * (self.t / PERIOD_Y * TAU + self.phase.1).sin()) as i32;

        // Slow breathing of the outline brightness between 65% and 100%.
        let breath = 0.825 + 0.175 * (self.t / 9.0 * TAU).sin();
        let lx = ox + (cw - self.lw as i32) / 2;
        let line = scale(rgb(190, 205, 255), breath);
        for &(x, y) in &self.outline {
            fb.put(lx + x as i32, oy + y as i32, line);
        }
        for &(x, y, ph) in &self.dust {
            let tw = (self.t / 3.0 * TAU + ph).sin();
            if tw > 0.2 {
                fb.put(lx + x as i32, oy + y as i32, scale(rgb(255, 250, 225), 0.35 + 0.65 * (tw - 0.2) / 0.8));
            }
        }

        let s = self.text_scale;
        let tw = font::text_width(BRAND[0], s);
        let tx = ox + (cw - tw) / 2;
        let mut ty = oy + self.lh as i32 + (self.lh / 8) as i32;
        let grey = rgb(150, 150, 150);
        for line in BRAND {
            font::draw_text(fb, tx, ty, s, grey, line);
            ty += ((GLYPH_H + 1) * s) as i32;
        }
    }
    fn wants_fps(&self) -> u32 {
        10
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(w: u32, h: u32, n: usize) -> Vec<Framebuffer> {
        let mut p = Midnight::default();
        let mut rng = Rng::new(11);
        p.init(w, h, 0.05, &mut rng);
        (0..n)
            .map(|_| {
                let mut fb = Framebuffer::new(w, h);
                p.step(Duration::from_millis(100), &mut fb, &mut rng);
                fb
            })
            .collect()
    }

    #[test]
    fn pbm_parses() {
        let (m, body) = parse_pbm(LOGO_PBM).expect("embedded PBM");
        assert_eq!((m.w, m.h), (240, 200));
        assert_eq!(body.len(), 30 * 200);
        // Corners are background, the middle of the crescent is set.
        assert!(!mask_bit(m, body, 0, 0));
        assert!(mask_bit(m, body, 55, 100));
    }

    #[test]
    fn budget_and_motion() {
        let f = frames(1280, 800, 30);
        for fb in &f {
            let lit = fb.lit_fraction();
            assert!(lit > 0.0 && lit <= 0.05, "lit {lit}");
        }
        assert_ne!(f[0], f[29]);
    }

    #[test]
    fn sizes() {
        for (w, h) in [(1, 1), (10, 10), (3840, 2160)] {
            assert_eq!(frames(w, h, 2).len(), 2);
        }
    }

    #[test]
    fn bad_pbm_is_rejected() {
        assert!(parse_pbm(b"P1\n2 2\n").is_none());
        assert!(parse_pbm(b"P4\n240 200\n").is_none());
    }
}
