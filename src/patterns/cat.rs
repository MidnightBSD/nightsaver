//! ASCII-art cat pattern. An original sitting cat drawn with the bitmap font: it blinks,
//! swishes its tail, dozes off with drifting z's, and now and then watches a mouse run by.
//! Text is sparse by nature and the scene drifts slowly, which keeps it OLED friendly.
use super::{Pattern, Rng};
use crate::render::font::{self, GLYPH_H, GLYPH_W};
use crate::render::{Framebuffer, rgb, scale};
use std::f32::consts::TAU;
use std::time::Duration;

/// Body, 17 columns. Row 1 holds the eyes (`o`), replaced by `-` when closed.
const BODY: [&str; 7] = [
    r"    /\_____/\    ",
    r"   /  o   o  \   ",
    r"  ( ==  ^  == )  ",
    r"   )         (   ",
    r"  (           )  ",
    r" ( (  )   (  ) ) ",
    r"(__(__)___(__)__)",
];
const EYE_ROW: usize = 1;
const BODY_W: usize = 17;

/// Tail poses, 4 columns wide, drawn to the right of body rows 3..6.
const TAILS: [[&str; 4]; 3] = [
    ["    ", "  _ ", " / )", "(_/ "],
    ["    ", " ,-.", "(  )", " `-'"],
    [" _  ", "( \\ ", " ) )", "(_/ "],
];
const TAIL_W: usize = 4;
const TAIL_ROW: usize = 3;

const MOUSE_LEFT: &str = "<:3 )~~";
const MOUSE_RIGHT: &str = "~~( 3:>";

/// Drift periods in seconds for the Lissajous path (x, y).
const PERIOD_X: f32 = 480.0;
const PERIOD_Y: f32 = 311.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mood {
    Awake,
    Asleep,
}

#[derive(Debug, Clone, Copy)]
struct Mouse {
    /// Progress across the screen, 0.0..1.0.
    pos: f32,
    to_right: bool,
}

#[derive(Debug, Default)]
pub struct Cat {
    width: u32,
    height: u32,
    scale: u32,
    t: f32,
    phase: (f32, f32),
    mood: Option<Mood>,
    /// Seconds until the mood changes.
    mood_left: f32,
    /// Seconds until the next blink; negative while the eyes are shut.
    blink: f32,
    tail: usize,
    tail_left: f32,
    mouse: Option<Mouse>,
    mouse_in: f32,
    /// Phase of the floating z's while asleep.
    zzz: f32,
}

impl Cat {
    fn set_mood(&mut self, mood: Mood, rng: &mut Rng) {
        self.mood = Some(mood);
        self.mood_left = match mood {
            Mood::Awake => 25.0 + 30.0 * rng.next_f32(),
            Mood::Asleep => 15.0 + 25.0 * rng.next_f32(),
        };
        self.blink = 2.0 + 4.0 * rng.next_f32();
        self.zzz = 0.0;
    }

    fn eyes_open(&self) -> bool {
        self.mood == Some(Mood::Awake) && self.blink >= 0.0
    }

    fn advance(&mut self, dts: f32, rng: &mut Rng) {
        let Some(mood) = self.mood else {
            self.set_mood(Mood::Awake, rng);
            return;
        };
        self.mood_left -= dts;
        if self.mood_left <= 0.0 {
            let next = if mood == Mood::Awake { Mood::Asleep } else { Mood::Awake };
            self.set_mood(next, rng);
        }
        match mood {
            Mood::Awake => {
                // Blink: eyes shut for 0.15 s every few seconds.
                self.blink -= dts;
                if self.blink < -0.15 {
                    self.blink = 2.0 + 5.0 * rng.next_f32();
                }
                self.tail_left -= dts;
                if self.tail_left <= 0.0 {
                    self.tail = (self.tail + 1 + rng.next_u32() as usize % 2) % TAILS.len();
                    self.tail_left = 0.5 + 1.0 * rng.next_f32();
                }
                self.mouse_in -= dts;
                if self.mouse.is_none() && self.mouse_in <= 0.0 {
                    self.mouse = Some(Mouse { pos: 0.0, to_right: rng.next_u32().is_multiple_of(2) });
                    self.mouse_in = 20.0 + 40.0 * rng.next_f32();
                }
            }
            Mood::Asleep => {
                self.zzz += dts;
            }
        }
        if let Some(m) = &mut self.mouse {
            m.pos += dts / 6.0;
            if m.pos >= 1.0 {
                self.mouse = None;
            }
        }
    }
}

impl Pattern for Cat {
    fn name(&self) -> &'static str {
        "cat"
    }
    fn init(&mut self, width: u32, height: u32, _lit_budget: f32, rng: &mut Rng) {
        self.width = width;
        self.height = height;
        self.t = 0.0;
        self.phase = (rng.next_f32() * TAU, rng.next_f32() * TAU);
        // Cat about a third of the screen width.
        let unit = font::text_width("x".repeat(BODY_W + TAIL_W).as_str(), 1).max(1) as u32;
        self.scale = (width / 3 / unit).max(1);
        self.mood = None;
        self.mouse = None;
        self.mouse_in = 8.0 + 20.0 * rng.next_f32();
        self.advance(0.0, rng);
    }
    fn step(&mut self, dt: Duration, fb: &mut Framebuffer, rng: &mut Rng) {
        let dts = dt.as_secs_f32().min(0.5);
        self.t += dts;
        self.advance(dts, rng);

        let s = self.scale.max(1);
        let adv = ((GLYPH_W + 1) * s) as i32;
        let row_h = ((GLYPH_H + 1) * s) as i32;
        let cw = (BODY_W + TAIL_W) as i32 * adv;
        let ch = BODY.len() as i32 * row_h;
        // Leave two rows above for the z's and one below for the mouse.
        let total_h = ch + 3 * row_h;

        let (w, h) = (fb.width as i32, fb.height as i32);
        let ax = ((w - cw).max(0) as f32) / 2.0;
        let ay = ((h - total_h).max(0) as f32) / 2.0;
        let ox = (ax + ax * (self.t / PERIOD_X * TAU + self.phase.0).sin()) as i32;
        let oy = (ay + ay * (self.t / PERIOD_Y * TAU + self.phase.1).sin()) as i32 + 2 * row_h;

        let fur = rgb(175, 165, 145);
        let eye = rgb(120, 230, 130);
        let open = self.eyes_open();
        for (r, line) in BODY.iter().enumerate() {
            let y = oy + r as i32 * row_h;
            for (c, chr) in line.chars().enumerate() {
                if chr == ' ' {
                    continue;
                }
                let x = ox + c as i32 * adv;
                if r == EYE_ROW && chr == 'o' {
                    let (glyph, col) = if open { ('o', eye) } else { ('-', fur) };
                    font::draw_char(fb, x, y, s, col, glyph);
                } else {
                    font::draw_char(fb, x, y, s, fur, chr);
                }
            }
        }
        let tail = TAILS[self.tail % TAILS.len()];
        for (r, line) in tail.iter().enumerate() {
            let y = oy + (TAIL_ROW + r) as i32 * row_h;
            font::draw_text(fb, ox + BODY_W as i32 * adv, y, s, fur, line);
        }

        if self.mood == Some(Mood::Asleep) {
            // Three z's rising from the head, each fading out over a 3 s cycle.
            let head_x = ox + 12 * adv;
            for k in 0..3 {
                let p = ((self.zzz / 3.0 + k as f32 / 3.0).fract()) * 1.0;
                let col = scale(rgb(150, 160, 200), (1.0 - p).max(0.0));
                let y = oy - (p * 2.0 * row_h as f32) as i32;
                let x = head_x + (p * 2.0 * adv as f32) as i32 + k * adv / 2;
                font::draw_char(fb, x, y, s, col, if k == 2 { 'Z' } else { 'z' });
            }
        }

        if let Some(m) = self.mouse {
            let text = if m.to_right { MOUSE_RIGHT } else { MOUSE_LEFT };
            let tw = font::text_width(text, s);
            let span = (w + tw) as f32;
            let x = if m.to_right { (m.pos * span) as i32 - tw } else { w - (m.pos * span) as i32 };
            let y = oy + ch;
            font::draw_text(fb, x, y, s, rgb(140, 140, 140), text);
        }
    }
    fn wants_fps(&self) -> u32 {
        10
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(w: u32, h: u32, n: usize, dt_ms: u64) -> Vec<Framebuffer> {
        let mut p = Cat::default();
        let mut rng = Rng::new(5);
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
    fn budget_and_content() {
        for fb in frames(1280, 800, 40, 100) {
            let lit = fb.lit_fraction();
            assert!(lit > 0.0 && lit <= 0.05, "lit {lit}");
        }
    }

    #[test]
    fn animates_over_time() {
        // Half-second steps over two minutes cover blinks, tail moves, sleep and a mouse.
        let f = frames(1280, 800, 240, 500);
        assert_ne!(f[0], f[239]);
        assert!(f.windows(2).filter(|p| p[0] != p[1]).count() > 50);
    }

    #[test]
    fn moods_alternate() {
        let mut p = Cat::default();
        let mut rng = Rng::new(9);
        p.init(640, 480, 0.05, &mut rng);
        let mut seen_sleep = false;
        let mut seen_awake = false;
        for _ in 0..600 {
            p.advance(0.5, &mut rng);
            match p.mood {
                Some(Mood::Asleep) => seen_sleep = true,
                Some(Mood::Awake) => seen_awake = true,
                None => {}
            }
        }
        assert!(seen_sleep && seen_awake);
    }

    #[test]
    fn sizes() {
        for (w, h) in [(1, 1), (10, 10), (3840, 2160)] {
            assert_eq!(frames(w, h, 2, 100).len(), 2);
        }
    }
}
