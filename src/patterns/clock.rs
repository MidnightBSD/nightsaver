//! Clock pattern. Owner: Sonnet D.
use super::{Pattern, Rng};
use crate::render::font::{self, GLYPH_H};
use crate::render::{Framebuffer, rgb};
use std::f32::consts::TAU;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Drift periods in seconds for the Lissajous path (x, y).
const PERIOD_X: f32 = 600.0;
const PERIOD_Y: f32 = 377.0;

#[derive(Debug, Default)]
pub struct Clock {
    width: u32,
    height: u32,
    scale: u32,
    t: f32,
    phase: (f32, f32),
}

/// Local (hour, minute, seconds-with-fraction) using libc `localtime_r`.
fn local_hms() -> (u32, u32, f32) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs() as libc::time_t;
    // SAFETY: `tm` is plain-old-data for which all-zero is valid; `localtime_r` only
    // reads `secs` and writes into the `tm` we own.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&secs, &mut tm).is_null() {
            return (0, 0, 0.0);
        }
        tm
    };
    let h = tm.tm_hour.clamp(0, 23) as u32;
    let m = tm.tm_min.clamp(0, 59) as u32;
    (h, m, tm.tm_sec.clamp(0, 60) as f32 + now.subsec_nanos() as f32 / 1e9)
}

impl Pattern for Clock {
    fn name(&self) -> &'static str {
        "clock"
    }
    fn init(&mut self, width: u32, height: u32, _lit_budget: f32, rng: &mut Rng) {
        self.width = width;
        self.height = height;
        let unit = font::text_width("00:00", 1).max(1) as u32;
        self.scale = (width / 6 / unit).max(1);
        self.t = 0.0;
        self.phase = (rng.next_f32() * TAU, rng.next_f32() * TAU);
    }
    fn step(&mut self, dt: Duration, fb: &mut Framebuffer, _rng: &mut Rng) {
        self.t += dt.as_secs_f32();
        let s = self.scale.max(1);
        let (h, m, sec) = local_hms();
        let text = format!("{h:02}:{m:02}");
        let tw = font::text_width(&text, s);
        let th = (GLYPH_H * s) as i32;
        // Reserve room below the text for the seconds dot.
        let dot_gap = (2 * s) as i32;
        let total_h = th + dot_gap + 2;

        let (w, hh) = (fb.width as i32, fb.height as i32);
        let ax = ((w - tw).max(0) as f32) / 2.0;
        let ay = ((hh - total_h).max(0) as f32) / 2.0;
        let x = ax + ax * (self.t / PERIOD_X * TAU + self.phase.0).sin();
        let y = ay + ay * (self.t / PERIOD_Y * TAU + self.phase.1).sin();
        let (x, y) = (x as i32, y as i32);

        font::draw_text(fb, x, y, s, rgb(140, 140, 140), &text);
        // Seconds indicator: 2x2 dot travelling along the text width once a minute.
        let frac = (sec / 60.0).clamp(0.0, 1.0);
        let dx = x + ((tw - 2).max(0) as f32 * frac) as i32;
        fb.fill_rect(dx, y + th + dot_gap, 2, 2, rgb(140, 140, 140));
    }
    fn wants_fps(&self) -> u32 {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(w: u32, h: u32, frames: usize, dt: Duration) -> Vec<Framebuffer> {
        let mut p = Clock::default();
        let mut rng = Rng::new(7);
        p.init(w, h, 0.05, &mut rng);
        (0..frames)
            .map(|_| {
                let mut fb = Framebuffer::new(w, h);
                p.step(dt, &mut fb, &mut rng);
                fb
            })
            .collect()
    }

    #[test]
    fn budget_and_content() {
        for fb in run(1280, 800, 5, Duration::from_millis(500)) {
            assert!(fb.lit_fraction() > 0.0);
            assert!(fb.lit_fraction() <= 0.06);
        }
    }

    #[test]
    fn differs_after_one_second() {
        let f = run(1280, 800, 3, Duration::from_millis(500));
        assert_ne!(f[0], f[2]);
    }

    #[test]
    fn sizes() {
        for (w, h) in [(1, 1), (10, 10), (3840, 2160)] {
            let f = run(w, h, 2, Duration::from_secs(1));
            assert_eq!(f.len(), 2);
        }
    }

    #[test]
    fn fps() {
        assert_eq!(Clock::default().wants_fps(), 2);
    }
}
