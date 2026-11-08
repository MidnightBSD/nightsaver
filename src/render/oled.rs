//! OLED protection post-pass applied to every frame. Owner: Sonnet D.
//! - luma cap (`max_brightness`)
//! - slow random-walk whole-frame offset every `shift_interval`
//! - periodic full-black rest interlude
//! - per-pattern lit-pixel budget hint

use super::Framebuffer;
use crate::config::OledConfig;
use std::time::Duration;

#[derive(Debug)]
pub struct OledGuard {
    cfg: OledConfig,
    elapsed: Duration,
    pub offset: (i32, i32),
    target: (i32, i32),
    next_shift: Duration,
    resting: bool,
    rng: u64,
}

impl OledGuard {
    pub fn new(cfg: OledConfig) -> Self {
        let next_shift = cfg.shift_interval;
        OledGuard {
            cfg,
            elapsed: Duration::ZERO,
            offset: (0, 0),
            target: (0, 0),
            next_shift,
            resting: false,
            rng: 0x9E37_79B9_7F4A_7C15,
        }
    }
    pub fn config(&self) -> &OledConfig {
        &self.cfg
    }

    fn next_rand(&mut self) -> u32 {
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u32
    }

    /// Advance clocks; returns true if we are in a rest (all-black) interlude.
    pub fn tick(&mut self, dt: Duration) -> bool {
        self.elapsed += dt;
        let max = i32::from(self.cfg.shift_max_px);

        // Pick a new random-walk target each shift interval.
        if self.elapsed >= self.next_shift {
            let step = if self.cfg.shift_interval.is_zero() {
                Duration::from_secs(1)
            } else {
                self.cfg.shift_interval
            };
            while self.next_shift <= self.elapsed {
                self.next_shift += step;
            }
            // Seed from elapsed so the walk differs between runs.
            self.rng ^= (self.elapsed.as_nanos() as u64) | 1;
            if self.rng == 0 {
                self.rng = 1;
            }
            let span = (2 * max + 1) as u32;
            let dx = (self.next_rand() % span) as i32 - max;
            let dy = (self.next_rand() % span) as i32 - max;
            self.target = ((self.target.0 + dx).clamp(-max, max), (self.target.1 + dy).clamp(-max, max));
        }
        // Keep the current offset valid even if config shrank, then glide 1 px per tick.
        self.target = (self.target.0.clamp(-max, max), self.target.1.clamp(-max, max));
        self.offset.0 += (self.target.0 - self.offset.0).signum();
        self.offset.1 += (self.target.1 - self.offset.1).signum();

        let interval = self.cfg.rest_interval;
        let rest = self.cfg.rest_duration;
        self.resting = if interval.is_zero() || rest.is_zero() {
            false
        } else {
            let period = (interval + rest).as_nanos();
            self.elapsed.as_nanos() % period >= interval.as_nanos()
        };
        self.resting
    }

    /// Apply cap + offset (and blackout if resting) in place.
    pub fn apply(&self, fb: &mut Framebuffer) {
        if self.resting {
            fb.clear();
            return;
        }
        let w = fb.width as usize;
        let h = fb.height as usize;
        if w == 0 || h == 0 {
            return;
        }
        let limit = (self.cfg.max_brightness.clamp(0.0, 1.0) * 255.0).round() as u32;
        let (dx, dy) = self.offset;

        if dx == 0 && dy == 0 {
            if limit < 255 {
                cap_row(&mut fb.pixels, limit);
            }
            return;
        }

        // Row order chosen so sources are read before being overwritten.
        let do_row = |y: usize, pixels: &mut [u32]| {
            let sy = y as i64 - i64::from(dy);
            let row = y * w;
            if sy < 0 || sy >= h as i64 || dx.unsigned_abs() as usize >= w {
                pixels[row..row + w].fill(0);
                return;
            }
            let src = sy as usize * w;
            let adx = dx.unsigned_abs() as usize;
            if dx >= 0 {
                pixels.copy_within(src..src + w - adx, row + adx);
                pixels[row..row + adx].fill(0);
                if limit < 255 {
                    cap_row(&mut pixels[row + adx..row + w], limit);
                }
            } else {
                pixels.copy_within(src + adx..src + w, row);
                pixels[row + w - adx..row + w].fill(0);
                if limit < 255 {
                    cap_row(&mut pixels[row..row + w - adx], limit);
                }
            }
        };
        if dy > 0 {
            for y in (0..h).rev() {
                do_row(y, &mut fb.pixels);
            }
        } else {
            for y in 0..h {
                do_row(y, &mut fb.pixels);
            }
        }
    }
}

/// Scale down any pixel whose Rec.601-ish luma exceeds `limit` (0..=255), preserving hue.
#[inline]
fn cap_row(px: &mut [u32], limit: u32) {
    for p in px.iter_mut() {
        let v = *p;
        if v & 0x00ff_ffff == 0 {
            continue;
        }
        let r = (v >> 16) & 0xff;
        let g = (v >> 8) & 0xff;
        let b = v & 0xff;
        // 0.299, 0.587, 0.114 scaled to 256 (77 + 150 + 29).
        let luma = (77 * r + 150 * g + 29 * b) >> 8;
        if luma > limit {
            let r = r * limit / luma;
            let g = g * limit / luma;
            let b = b * limit / luma;
            *p = (r << 16) | (g << 8) | b;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> OledConfig {
        OledConfig {
            max_brightness: 0.35,
            shift_interval: Duration::from_secs(60),
            shift_max_px: 16,
            rest_interval: Duration::from_secs(600),
            rest_duration: Duration::from_secs(20),
            lit_budget: 0.05,
        }
    }

    fn luma(p: u32) -> u32 {
        (299 * ((p >> 16) & 0xff) + 587 * ((p >> 8) & 0xff) + 114 * (p & 0xff)) / 1000
    }

    #[test]
    fn cap_holds_for_white() {
        let g = OledGuard::new(cfg());
        let mut fb = Framebuffer::new(32, 32);
        fb.pixels.fill(0xffffff);
        g.apply(&mut fb);
        let limit = (0.35f32 * 255.0).round() as u32;
        for &p in &fb.pixels {
            assert!(luma(p) <= limit + 1, "luma {}", luma(p));
            assert!(luma(p) + 3 >= limit);
        }
    }

    #[test]
    fn cap_preserves_hue_and_dim_pixels() {
        let g = OledGuard::new(cfg());
        let mut fb = Framebuffer::new(2, 1);
        fb.pixels[0] = 0xff8000;
        fb.pixels[1] = 0x101010;
        g.apply(&mut fb);
        let (r, gr, b) = (fb.pixels[0] >> 16, (fb.pixels[0] >> 8) & 0xff, fb.pixels[0] & 0xff);
        assert!(r < 0xff && r > 0 && b == 0);
        assert!((r as i32 - 2 * gr as i32).abs() <= 3);
        assert_eq!(fb.pixels[1], 0x101010);
    }

    #[test]
    fn blank_stays_blank() {
        let mut g = OledGuard::new(cfg());
        let mut fb = Framebuffer::new(16, 16);
        g.offset = (3, -2);
        g.apply(&mut fb);
        assert_eq!(fb.lit_fraction(), 0.0);
    }

    #[test]
    fn resting_blanks() {
        let mut g = OledGuard::new(cfg());
        assert!(!g.tick(Duration::from_secs(1)));
        assert!(g.tick(Duration::from_secs(605)));
        let mut fb = Framebuffer::new(8, 8);
        fb.pixels.fill(0x808080);
        g.apply(&mut fb);
        assert_eq!(fb.lit_fraction(), 0.0);
        assert!(!g.tick(Duration::from_secs(20)));
    }

    #[test]
    fn offset_moves_pixel() {
        for &(dx, dy) in &[(3, 2), (-3, -2), (4, -1), (-2, 5), (0, 0), (0, 3), (-4, 0)] {
            let mut g = OledGuard::new(cfg());
            g.offset = (dx, dy);
            let mut fb = Framebuffer::new(20, 20);
            fb.put(10, 10, 0x202020);
            fb.put(0, 0, 0x202020);
            g.apply(&mut fb);
            assert_eq!(fb.get(10 + dx, 10 + dy), Some(0x202020), "{dx},{dy}");
            let lit = fb.pixels.iter().filter(|&&p| p != 0).count();
            let corner_survives = dx >= 0 && dy >= 0;
            assert_eq!(lit, if corner_survives { 2 } else { 1 });
        }
    }

    #[test]
    fn offset_larger_than_frame_blanks() {
        let mut g = OledGuard::new(cfg());
        g.offset = (50, 0);
        let mut fb = Framebuffer::new(10, 10);
        fb.pixels.fill(0x303030);
        g.apply(&mut fb);
        assert_eq!(fb.lit_fraction(), 0.0);
    }

    #[test]
    fn walk_is_gradual_and_bounded() {
        let mut c = cfg();
        c.shift_interval = Duration::from_millis(100);
        c.rest_interval = Duration::ZERO;
        let mut g = OledGuard::new(c);
        let mut prev = g.offset;
        let mut moved = false;
        for _ in 0..2000 {
            g.tick(Duration::from_millis(33));
            assert!((g.offset.0 - prev.0).abs() <= 1 && (g.offset.1 - prev.1).abs() <= 1);
            assert!(g.offset.0.abs() <= 16 && g.offset.1.abs() <= 16);
            moved |= g.offset != (0, 0);
            prev = g.offset;
        }
        assert!(moved);
    }
}
