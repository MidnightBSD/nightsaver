//! Starfield pattern. Owner: Sonnet D.
use super::{Pattern, Rng, lit_budget_pixels};
use crate::render::{Framebuffer, rgb, scale};
use std::f32::consts::TAU;
use std::time::Duration;

const MAX_STARS: usize = 400;

#[derive(Debug, Clone, Copy)]
struct Star {
    angle: f32,
    /// Distance from the vanishing point in pixels.
    r: f32,
    /// Per-star speed factor.
    speed: f32,
}

#[derive(Debug, Default)]
pub struct Starfield {
    width: u32,
    height: u32,
    rmax: f32,
    t: f32,
    stars: Vec<Star>,
}

impl Starfield {
    fn spawn(&self, rng: &mut Rng, anywhere: bool) -> Star {
        let r = if anywhere {
            rng.next_f32() * self.rmax * 0.9
        } else {
            (0.01 + 0.04 * rng.next_f32()) * self.rmax
        };
        Star { angle: rng.next_f32() * TAU, r, speed: 0.6 + 0.8 * rng.next_f32() }
    }
    fn vanishing_point(&self) -> (f32, f32) {
        let (w, h) = (self.width as f32, self.height as f32);
        (w / 2.0 + w * 0.12 * (self.t / 37.0).sin(), h / 2.0 + h * 0.12 * (self.t / 53.0).cos())
    }
}

impl Pattern for Starfield {
    fn name(&self) -> &'static str {
        "starfield"
    }
    fn init(&mut self, width: u32, height: u32, lit_budget: f32, rng: &mut Rng) {
        self.width = width;
        self.height = height;
        self.t = 0.0;
        self.rmax = (width as f32).hypot(height as f32).max(1.0);
        // A close star is a streak of up to ~3x16 px; keep the total area within 60% of the budget.
        let n = ((lit_budget_pixels(width, height, lit_budget) as f32 * 0.6 / 48.0) as usize).min(MAX_STARS);
        self.stars = (0..n).map(|_| self.spawn(rng, true)).collect();
    }
    fn step(&mut self, dt: Duration, fb: &mut Framebuffer, rng: &mut Rng) {
        let dts = dt.as_secs_f32().min(0.1);
        self.t += dts;
        let (vx, vy) = self.vanishing_point();
        let (w, h) = (fb.width as f32, fb.height as f32);
        for i in 0..self.stars.len() {
            let mut s = self.stars[i];
            let r0 = s.r;
            s.r += dts * s.speed * (self.rmax * 0.03 + s.r * 0.35);
            let (ca, sa) = (s.angle.cos(), s.angle.sin());
            let x = vx + ca * s.r;
            let y = vy + sa * s.r;
            if x < 0.0 || y < 0.0 || x >= w || y >= h {
                self.stars[i] = self.spawn(rng, false);
                continue;
            }
            self.stars[i] = s;
            // Depth: farther (near the vanishing point) is dimmer, smaller and slower.
            let d = (s.r / (self.rmax * 0.45)).clamp(0.0, 1.0);
            let bright = 0.45 + 0.55 * d;
            let head = scale(rgb(235, 240, 255), bright);
            let tail = scale(rgb(150, 170, 255), bright * 0.4);
            let size = if d > 0.7 {
                3
            } else if d > 0.3 {
                2
            } else {
                1
            };
            // Streak from the previous position, lengthened with depth for a warp feel.
            let trail = (s.r - r0) * (1.0 + 3.0 * d) + 1.5 * d;
            let tr = (s.r - trail).max(0.0);
            let (tx, ty) = (vx + ca * tr, vy + sa * tr);
            streak(fb, (tx, ty), (x, y), size, (tail, head));
        }
    }
    fn wants_fps(&self) -> u32 {
        30
    }
}

/// Draw a short line from `from` to `to`, fading between `colors.0` and `colors.1`, `size` px thick.
fn streak(fb: &mut Framebuffer, from: (f32, f32), to: (f32, f32), size: u32, colors: (u32, u32)) {
    let ((x0, y0), (x1, y1)) = (from, to);
    let (dx, dy) = (x1 - x0, y1 - y0);
    let steps = dx.abs().max(dy.abs()).ceil().clamp(1.0, 64.0);
    let n = steps as u32;
    let half = (size / 2) as f32;
    for k in 0..=n {
        let f = k as f32 / steps;
        let (x, y) = (x0 + dx * f - half, y0 + dy * f - half);
        let col = lerp(colors.0, colors.1, f);
        fb.fill_rect(x.round() as i32, y.round() as i32, size, size, col);
    }
}

fn lerp(a: u32, b: u32, f: f32) -> u32 {
    let ch = |sh: u32| {
        let (ca, cb) = (((a >> sh) & 0xff) as f32, ((b >> sh) & 0xff) as f32);
        ((ca + (cb - ca) * f).round().clamp(0.0, 255.0) as u32) << sh
    };
    ch(16) | ch(8) | ch(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(w: u32, h: u32, n: usize, budget: f32) -> Vec<Framebuffer> {
        let mut p = Starfield::default();
        let mut rng = Rng::new(3);
        p.init(w, h, budget, &mut rng);
        (0..n)
            .map(|_| {
                let mut fb = Framebuffer::new(w, h);
                p.step(Duration::from_millis(33), &mut fb, &mut rng);
                fb
            })
            .collect()
    }

    #[test]
    fn budget_and_motion() {
        let f = frames(1280, 800, 60, 0.05);
        for fb in &f {
            assert!(fb.lit_fraction() <= 0.06);
        }
        assert!(f[0].lit_fraction() > 0.0);
        assert_ne!(f[0], f[1]);
        assert_ne!(f[10], f[59]);
    }

    #[test]
    fn sizes() {
        for (w, h) in [(1, 1), (10, 10), (3840, 2160)] {
            assert_eq!(frames(w, h, 2, 0.05).len(), 2);
        }
    }

    #[test]
    fn long_run_keeps_respawning() {
        let f = frames(320, 200, 2000, 0.05);
        assert!(f.last().unwrap().lit_fraction() > 0.0);
    }
}
