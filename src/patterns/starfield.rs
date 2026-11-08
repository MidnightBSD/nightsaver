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
        // Stars cover up to 4 px (2x2) when close; keep total area within 30% of the budget.
        let n = ((lit_budget_pixels(width, height, lit_budget) as f32 * 0.3 / 4.0) as usize).min(MAX_STARS);
        self.stars = (0..n).map(|_| self.spawn(rng, true)).collect();
    }
    fn step(&mut self, dt: Duration, fb: &mut Framebuffer, rng: &mut Rng) {
        let dts = dt.as_secs_f32();
        self.t += dts;
        let (vx, vy) = self.vanishing_point();
        let (w, h) = (fb.width as f32, fb.height as f32);
        for i in 0..self.stars.len() {
            let mut s = self.stars[i];
            s.r += dts * s.speed * (self.rmax * 0.02 + s.r * 0.25);
            let x = vx + s.angle.cos() * s.r;
            let y = vy + s.angle.sin() * s.r;
            if x < 0.0 || y < 0.0 || x >= w || y >= h {
                self.stars[i] = self.spawn(rng, false);
                continue;
            }
            self.stars[i] = s;
            // Depth: farther (near the vanishing point) is dimmer and smaller.
            let d = (s.r / (self.rmax * 0.5)).clamp(0.0, 1.0);
            let col = scale(rgb(255, 255, 255), 0.2 + 0.8 * d);
            let size = if d > 0.55 { 2 } else { 1 };
            fb.fill_rect(x as i32, y as i32, size, size, col);
        }
    }
    fn wants_fps(&self) -> u32 {
        30
    }
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
