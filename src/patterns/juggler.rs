//! ASCII-art juggler pattern. A stick figure performs a three-ball cascade: the balls follow
//! computed parabolic arcs between the hands (siteswap 3: two beats in the air, one beat in
//! the hand), the catching hand scoops, and the figure sways and drifts slowly across the
//! screen. Sparse glyphs on black keep it OLED friendly.
use super::{Pattern, Rng};
use crate::render::font::{self, GLYPH_H, GLYPH_W};
use crate::render::{Framebuffer, rgb, scale};
use std::f32::consts::{PI, TAU};
use std::time::Duration;

/// The figure, 7 columns. Hands are the outer characters of row 2.
const BODY: [&str; 6] = [
    "   O   ", //
    "  /|\\  ", //
    " - | - ", //
    "   |   ", //
    "  / \\  ", //
    " /   \\ ", //
];
const BODY_W: usize = 7;
/// Rows reserved above the body for the balls' arcs.
const SKY_ROWS: i32 = 3;
/// Hand positions in cell coordinates (column, row).
const LEFT_HAND: (f32, f32) = (1.0, 2.0);
const RIGHT_HAND: (f32, f32) = (5.0, 2.0);
/// Apex height of a throw above the hands, in rows.
const THROW_HEIGHT: f32 = 4.0;
/// Seconds per beat (one throw).
const BEAT: f32 = 0.42;
const BALLS: usize = 3;
const BALL_COLORS: [u32; BALLS] = [0x00f0_6a6a, 0x0070_e080, 0x0070_90ff];

/// Drift periods in seconds for the Lissajous path (x, y).
const PERIOD_X: f32 = 500.0;
const PERIOD_Y: f32 = 323.0;

#[derive(Debug, Default)]
pub struct Juggler {
    width: u32,
    height: u32,
    scale: u32,
    t: f32,
    phase: (f32, f32),
    /// Beat offset so the pattern does not always start identically.
    beat0: f32,
}

/// Position of ball `k` at time `beats` (in beats) in cell coordinates, plus whether it is
/// resting in a hand (and which: 0 = left, 1 = right).
fn ball_position(k: usize, beats: f32) -> ((f32, f32), Option<usize>) {
    // Ball k is thrown at beats k, k+3, k+6, ...; throws alternate hands every beat.
    let since = beats - k as f32;
    let n = (since / 3.0).floor();
    let throw_beat = k as f32 + 3.0 * n;
    let from = (throw_beat as i64).rem_euclid(2) as usize;
    let to = 1 - from;
    let hands = [LEFT_HAND, RIGHT_HAND];
    let (fx, fy) = hands[from];
    let (tx, ty) = hands[to];
    let phase = since - 3.0 * n; // 0..3
    if phase < 2.0 {
        let u = phase / 2.0;
        let x = fx + (tx - fx) * u;
        let y = fy - 0.9 - THROW_HEIGHT * 4.0 * u * (1.0 - u);
        ((x, y), None)
    } else {
        // Dwell: the hand scoops inward and down, then back up to throw.
        let d = phase - 2.0; // 0..1
        let inward = if to == 0 { 1.0 } else { -1.0 };
        let x = tx + inward * 0.6 * (PI * d).sin();
        let y = ty - 0.9 + 0.5 * (PI * d).sin();
        ((x, y), Some(to))
    }
}

impl Pattern for Juggler {
    fn name(&self) -> &'static str {
        "juggler"
    }
    fn init(&mut self, width: u32, height: u32, _lit_budget: f32, rng: &mut Rng) {
        self.width = width;
        self.height = height;
        self.t = 0.0;
        self.phase = (rng.next_f32() * TAU, rng.next_f32() * TAU);
        self.beat0 = rng.next_f32() * 6.0;
        // Figure about a fifth of the width, and the whole act at most 70% of the height.
        let unit_w = ((GLYPH_W + 1) * BODY_W as u32).max(1);
        let rows = BODY.len() as u32 + SKY_ROWS as u32 + 1;
        let unit_h = ((GLYPH_H + 1) * rows).max(1);
        let by_w = width / 5 / unit_w;
        let by_h = height * 7 / 10 / unit_h;
        self.scale = by_w.min(by_h).max(1);
    }
    fn step(&mut self, dt: Duration, fb: &mut Framebuffer, _rng: &mut Rng) {
        self.t += dt.as_secs_f32().min(0.5);
        let s = self.scale.max(1);
        let cw = ((GLYPH_W + 1) * s) as f32;
        let ch = ((GLYPH_H + 1) * s) as f32;
        let total_w = (BODY_W as f32 * cw) as i32;
        let total_h = ((BODY.len() as i32 + SKY_ROWS + 1) as f32 * ch) as i32;

        let (w, h) = (fb.width as i32, fb.height as i32);
        let ax = ((w - total_w).max(0) as f32) / 2.0;
        let ay = ((h - total_h).max(0) as f32) / 2.0;
        let ox = (ax + ax * (self.t / PERIOD_X * TAU + self.phase.0).sin()) as i32;
        let oy = (ay + ay * (self.t / PERIOD_Y * TAU + self.phase.1).sin()) as i32;
        // Body origin: below the sky rows. The figure sways a little with the rhythm.
        let beats = self.t / BEAT + self.beat0;
        let sway = ((beats * PI).sin() * 0.15 * cw) as i32;
        let bx = ox + sway;
        let by = oy + (SKY_ROWS as f32 * ch) as i32;

        let skin = rgb(170, 165, 150);
        let hands: Vec<Option<usize>> = (0..BALLS).map(|k| ball_position(k, beats).1).collect();
        for (r, line) in BODY.iter().enumerate() {
            let y = by + (r as f32 * ch) as i32;
            for (c, chr) in line.chars().enumerate() {
                if chr == ' ' {
                    continue;
                }
                let mut x = bx + (c as f32 * cw) as i32;
                let mut y = y;
                // Hands follow the scoop of the ball they hold.
                if r == 2 && (c == 1 || c == 5) {
                    let side = if c == 1 { 0 } else { 1 };
                    if let Some(k) = hands.iter().position(|hd| *hd == Some(side)) {
                        let ((px, py), _) = ball_position(k, beats);
                        x = bx + (px * cw) as i32;
                        y = by + ((py + 0.9) * ch) as i32;
                    }
                }
                font::draw_char(fb, x, y, s, skin, chr);
            }
        }
        for (k, col) in BALL_COLORS.iter().enumerate() {
            let ((px, py), held) = ball_position(k, beats);
            let x = bx + (px * cw) as i32;
            let y = by + (py * ch) as i32;
            // Airborne balls glow a little brighter than held ones.
            let c = scale(*col, if held.is_some() { 0.75 } else { 1.0 });
            font::draw_char(fb, x, y, s, c, '*');
        }
    }
    fn wants_fps(&self) -> u32 {
        20
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(w: u32, h: u32, n: usize, dt_ms: u64) -> Vec<Framebuffer> {
        let mut p = Juggler::default();
        let mut rng = Rng::new(21);
        p.init(w, h, 0.05, &mut rng);
        (0..n)
            .map(|_| {
                let mut fb = Framebuffer::new(w, h);
                p.step(Duration::from_millis(dt_ms), &mut fb, &mut rng);
                fb
            })
            .collect()
    }

    #[test]
    fn cascade_is_consistent() {
        // At any instant at most one ball rests in each hand, and no ball is below the hands.
        for i in 0..300 {
            let beats = i as f32 * 0.05;
            let mut in_hand = [0, 0];
            for k in 0..BALLS {
                let ((_, y), held) = ball_position(k, beats);
                assert!(y <= LEFT_HAND.1, "ball {k} below hands at {beats}: y={y}");
                if let Some(h) = held {
                    in_hand[h] += 1;
                }
            }
            assert!(in_hand[0] <= 1 && in_hand[1] <= 1, "two balls in one hand at {beats}");
        }
    }

    #[test]
    fn balls_cross_sides() {
        // Ball 0 is thrown from the left at beat 0 and lands on the right by beat 2.
        let ((x0, _), _) = ball_position(0, 0.01);
        let ((x2, _), held) = ball_position(0, 2.01);
        assert!(x0 < 2.0 && x2 > 4.0, "x0={x0} x2={x2}");
        assert_eq!(held, Some(1));
    }

    #[test]
    fn budget_and_motion() {
        let f = frames(1280, 800, 40, 50);
        for fb in &f {
            let lit = fb.lit_fraction();
            assert!(lit > 0.0 && lit <= 0.05, "lit {lit}");
        }
        assert_ne!(f[0], f[1]);
        assert!(f.windows(2).all(|p| p[0] != p[1]), "every frame should move");
    }

    #[test]
    fn sizes() {
        for (w, h) in [(1, 1), (10, 10), (3840, 2160)] {
            assert_eq!(frames(w, h, 2, 50).len(), 2);
        }
    }
}
