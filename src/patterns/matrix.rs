//! Matrix pattern. Owner: Sonnet D.
use super::{Pattern, Rng, lit_budget_pixels};
use crate::render::font::{self, GLYPH_H, GLYPH_W};
use crate::render::{Framebuffer, rgb, scale};
use std::time::Duration;

const TRAIL: usize = 8;
const CHARSET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";

#[derive(Debug, Clone)]
struct Column {
    col: u32,
    /// Fractional row of the leading glyph.
    head: f32,
    /// Rows per second.
    speed: f32,
    glyphs: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct Matrix {
    scale: u32,
    cols: u32,
    rows: u32,
    cell_w: u32,
    cell_h: u32,
    active: Vec<Column>,
}

impl Matrix {
    fn spawn(&self, rng: &mut Rng, anywhere: bool) -> Column {
        let rows = self.rows as usize;
        let glyphs = (0..rows).map(|_| CHARSET[rng.next_u32() as usize % CHARSET.len()]).collect();
        let head =
            if anywhere { rng.next_f32() * rows as f32 } else { -(rng.next_f32() * 2.0 * TRAIL as f32) };
        Column {
            col: rng.next_u32() % self.cols.max(1),
            head,
            speed: 5.0 + 20.0 * rng.next_f32(),
            glyphs,
        }
    }
}

impl Pattern for Matrix {
    fn name(&self) -> &'static str {
        "matrix"
    }
    fn init(&mut self, width: u32, height: u32, lit_budget: f32, rng: &mut Rng) {
        self.scale = (height / 400).max(1);
        self.cell_w = (GLYPH_W + 1) * self.scale;
        self.cell_h = (GLYPH_H + 1) * self.scale;
        self.cols = width / self.cell_w;
        self.rows = height.div_ceil(self.cell_h);
        // Roughly half of a glyph cell is lit; a column shows TRAIL+1 glyphs.
        let per_col = ((TRAIL + 1) as f32 * (GLYPH_W * GLYPH_H * self.scale * self.scale) as f32 * 0.5).max(1.0);
        let by_budget = (lit_budget_pixels(width, height, lit_budget) as f32 / per_col) as usize;
        let by_density = (self.cols / 8).max(1) as usize;
        let n = if self.cols == 0 { 0 } else { by_density.min(by_budget) };
        self.active = (0..n).map(|_| self.spawn(rng, true)).collect();
    }
    fn step(&mut self, dt: Duration, fb: &mut Framebuffer, rng: &mut Rng) {
        let dts = dt.as_secs_f32();
        let s = self.scale.max(1);
        let lead = rgb(210, 255, 220);
        let body = rgb(0, 255, 70);
        for i in 0..self.active.len() {
            self.active[i].head += self.active[i].speed * dts;
            if self.active[i].head - (TRAIL as f32) > self.rows as f32 {
                self.active[i] = self.spawn(rng, false);
            }
            // Occasionally mutate a glyph so the trail shimmers.
            if rng.next_u32().is_multiple_of(4) && !self.active[i].glyphs.is_empty() {
                let k = rng.next_u32() as usize % self.active[i].glyphs.len();
                self.active[i].glyphs[k] = CHARSET[rng.next_u32() as usize % CHARSET.len()];
            }
            let c = &self.active[i];
            let head_row = c.head.floor() as i64;
            let x = (c.col * self.cell_w) as i32;
            for k in 0..=TRAIL {
                let row = head_row - k as i64;
                if row < 0 || row >= self.rows as i64 {
                    continue;
                }
                let colr = if k == 0 {
                    lead
                } else {
                    let f = 1.0 - k as f32 / (TRAIL + 1) as f32;
                    scale(body, f * f)
                };
                let y = (row as u32 * self.cell_h) as i32;
                font::draw_char(fb, x, y, s, colr, c.glyphs[row as usize] as char);
            }
        }
    }
    fn wants_fps(&self) -> u32 {
        15
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(w: u32, h: u32, n: usize, budget: f32) -> Vec<Framebuffer> {
        let mut p = Matrix::default();
        let mut rng = Rng::new(5);
        p.init(w, h, budget, &mut rng);
        (0..n)
            .map(|_| {
                let mut fb = Framebuffer::new(w, h);
                p.step(Duration::from_millis(66), &mut fb, &mut rng);
                fb
            })
            .collect()
    }

    #[test]
    fn budget_and_motion() {
        let f = frames(1280, 800, 100, 0.05);
        for fb in &f {
            assert!(fb.lit_fraction() <= 0.06);
        }
        assert!(f.iter().any(|fb| fb.lit_fraction() > 0.0));
        assert_ne!(f[0], f[1]);
        assert_ne!(f[1], f[2]);
    }

    #[test]
    fn sparse_columns() {
        let mut p = Matrix::default();
        let mut rng = Rng::new(1);
        p.init(1280, 800, 0.05, &mut rng);
        assert!(p.active.len() as u32 <= p.cols / 8);
        assert!(!p.active.is_empty());
    }

    #[test]
    fn sizes() {
        for (w, h) in [(1, 1), (10, 10), (3840, 2160)] {
            assert_eq!(frames(w, h, 2, 0.05).len(), 2);
        }
    }

    #[test]
    fn tiny_budget() {
        assert_eq!(frames(1280, 800, 3, 0.0).len(), 3);
    }
}
