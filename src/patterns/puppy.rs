//! ASCII-art puppy pattern. An original sitting puppy drawn with the bitmap font: it wags
//! its tail, pants, blinks, barks now and then, gets excited when a ball bounces past and
//! curls up for a nap with drifting z's. Text is sparse by nature and the scene drifts
//! slowly, which keeps it OLED friendly.
use super::{Pattern, Rng};
use crate::render::font::{self, GLYPH_H, GLYPH_W};
use crate::render::{Framebuffer, rgb, scale};
use std::f32::consts::{PI, TAU};
use std::time::Duration;

/// Body, 17 columns: floppy ears either side of the face, front paws at the bottom.
/// Row 2 holds the eyes (`o`), row 3 the nose (`Y`).
const BODY: [&str; 8] = [
    r"   __       __   ",
    r"  /  \_____/  \  ",
    r" (  ) o   o (  ) ",
    r"  \/    Y    \/  ",
    r"    \  `-'  /    ",
    r"    /       \    ",
    r"   ( |     | )   ",
    r"   (_|_____|_)   ",
];
const EYE_ROW: usize = 2;
const NOSE_ROW: usize = 3;
const BODY_W: usize = 17;
/// Where the tongue hangs out while panting (row, column).
const TONGUE: (usize, usize) = (5, 8);

/// Tail poses, 4 columns wide, drawn from column `TAIL_COL` over body rows 5..8. The last
/// pose is the resting tail used while asleep; the first three are the wag cycle.
const TAILS: [[&str; 3]; 4] = [
    ["  ,~", " /  ", "/   "],
    [" |  ", " |  ", "/   "],
    ["~.  ", "  \\ ", "/   "],
    ["    ", "    ", "/~~ "],
];
const WAG_POSES: usize = 3;
const REST_TAIL: usize = 3;
const TAIL_W: usize = 4;
const TAIL_COL: usize = 14;
const TAIL_ROW: usize = 5;
/// Columns the whole figure occupies (body plus the tail's overhang).
const FIGURE_W: usize = TAIL_COL + TAIL_W;
// The tail starts inside the body's blank right margin and overhangs it by one column.
const _: () = assert!(TAIL_COL < BODY_W && FIGURE_W > BODY_W);

const BARK: &str = "WOOF!";

/// Drift periods in seconds for the Lissajous path (x, y).
const PERIOD_X: f32 = 437.0;
const PERIOD_Y: f32 = 293.0;
/// Seconds the ball takes to cross the screen, and how often it bounces on the way.
const BALL_CROSSING: f32 = 7.0;
const BALL_BOUNCES: f32 = 5.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mood {
    Awake,
    Asleep,
}

#[derive(Debug, Clone, Copy)]
struct Ball {
    /// Progress across the screen, 0.0..1.0.
    pos: f32,
    to_right: bool,
}

#[derive(Debug, Default)]
pub struct Puppy {
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
    /// Seconds of panting left (tongue out) and seconds until the next bout.
    pant: f32,
    pant_in: f32,
    /// Seconds the current bark stays on screen and seconds until the next one.
    bark: f32,
    bark_in: f32,
    ball: Option<Ball>,
    ball_in: f32,
    /// Phase of the floating z's while asleep.
    zzz: f32,
}

impl Puppy {
    fn set_mood(&mut self, mood: Mood, rng: &mut Rng) {
        self.mood = Some(mood);
        self.mood_left = match mood {
            Mood::Awake => 30.0 + 30.0 * rng.next_f32(),
            Mood::Asleep => 12.0 + 20.0 * rng.next_f32(),
        };
        self.blink = 2.0 + 4.0 * rng.next_f32();
        self.pant = 0.0;
        self.pant_in = 3.0 + 6.0 * rng.next_f32();
        self.bark = 0.0;
        self.bark_in = 6.0 + 14.0 * rng.next_f32();
        self.zzz = 0.0;
        if mood == Mood::Asleep {
            self.tail = REST_TAIL;
        }
    }

    fn awake(&self) -> bool {
        self.mood == Some(Mood::Awake)
    }

    fn excited(&self) -> bool {
        self.awake() && (self.ball.is_some() || self.bark > 0.0)
    }

    fn advance(&mut self, dts: f32, rng: &mut Rng) {
        let Some(mut mood) = self.mood else {
            self.set_mood(Mood::Awake, rng);
            return;
        };
        self.mood_left -= dts;
        // A puppy does not doze off in the middle of a ball game.
        if self.mood_left <= 0.0 && self.ball.is_none() {
            mood = if mood == Mood::Awake { Mood::Asleep } else { Mood::Awake };
            self.set_mood(mood, rng);
        }
        match mood {
            Mood::Awake => {
                // Blink: eyes shut for 0.15 s every few seconds.
                self.blink -= dts;
                if self.blink < -0.15 {
                    self.blink = 2.0 + 5.0 * rng.next_f32();
                }
                // Wag: quick when excited, lazy otherwise.
                self.tail_left -= dts;
                if self.tail_left <= 0.0 {
                    self.tail = (self.tail + 1) % WAG_POSES;
                    self.tail_left = if self.excited() { 0.12 } else { 0.35 + 0.4 * rng.next_f32() };
                }
                if self.pant > 0.0 {
                    self.pant -= dts;
                } else {
                    self.pant_in -= dts;
                    if self.pant_in <= 0.0 {
                        self.pant = 3.0 + 5.0 * rng.next_f32();
                        self.pant_in = 4.0 + 8.0 * rng.next_f32();
                    }
                }
                if self.bark > 0.0 {
                    self.bark -= dts;
                } else {
                    self.bark_in -= dts;
                    if self.bark_in <= 0.0 {
                        self.bark = 0.9;
                        // Barks come thick and fast while the ball is in play.
                        self.bark_in = if self.ball.is_some() {
                            1.5 + 2.0 * rng.next_f32()
                        } else {
                            10.0 + 25.0 * rng.next_f32()
                        };
                    }
                }
                self.ball_in -= dts;
                if self.ball.is_none() && self.ball_in <= 0.0 {
                    self.ball = Some(Ball { pos: 0.0, to_right: rng.next_u32().is_multiple_of(2) });
                    self.ball_in = 25.0 + 40.0 * rng.next_f32();
                    self.bark_in = self.bark_in.min(1.0);
                }
            }
            Mood::Asleep => {
                self.zzz += dts;
            }
        }
        if let Some(b) = &mut self.ball {
            b.pos += dts / BALL_CROSSING;
            if b.pos >= 1.0 {
                self.ball = None;
            }
        }
    }
}

impl Pattern for Puppy {
    fn name(&self) -> &'static str {
        "puppy"
    }
    fn init(&mut self, width: u32, height: u32, _lit_budget: f32, rng: &mut Rng) {
        self.width = width;
        self.height = height;
        self.t = 0.0;
        self.phase = (rng.next_f32() * TAU, rng.next_f32() * TAU);
        // Puppy about a third of the screen width.
        let unit = font::text_width("x".repeat(FIGURE_W).as_str(), 1).max(1) as u32;
        self.scale = (width / 3 / unit).max(1);
        self.mood = None;
        self.tail = 0;
        self.tail_left = 0.0;
        self.ball = None;
        self.ball_in = 8.0 + 20.0 * rng.next_f32();
        self.advance(0.0, rng);
    }
    fn step(&mut self, dt: Duration, fb: &mut Framebuffer, rng: &mut Rng) {
        let dts = dt.as_secs_f32().min(0.5);
        self.t += dts;
        self.advance(dts, rng);

        let s = self.scale.max(1);
        let adv = ((GLYPH_W + 1) * s) as i32;
        let row_h = ((GLYPH_H + 1) * s) as i32;
        let cw = FIGURE_W as i32 * adv;
        let ch = BODY.len() as i32 * row_h;
        // Leave two rows above for the z's and the bark, and room below for the ball.
        let total_h = ch + 3 * row_h;

        let (w, h) = (fb.width as i32, fb.height as i32);
        let ax = ((w - cw).max(0) as f32) / 2.0;
        let ay = ((h - total_h).max(0) as f32) / 2.0;
        let ox = (ax + ax * (self.t / PERIOD_X * TAU + self.phase.0).sin()) as i32;
        let oy = (ay + ay * (self.t / PERIOD_Y * TAU + self.phase.1).sin()) as i32 + 2 * row_h;

        let fur = rgb(190, 150, 95);
        let eye = rgb(235, 185, 90);
        let nose = rgb(150, 110, 90);
        let awake = self.awake();
        let barking = awake && self.bark > 0.0;
        let open = awake && self.blink >= 0.0;
        for (r, line) in BODY.iter().enumerate() {
            let y = oy + r as i32 * row_h;
            for (c, chr) in line.chars().enumerate() {
                if chr == ' ' {
                    continue;
                }
                let x = ox + c as i32 * adv;
                if r == EYE_ROW && chr == 'o' {
                    // Happy squint while barking, shut while blinking or asleep.
                    let (glyph, col) = match (barking, open) {
                        (true, _) => ('^', eye),
                        (false, true) => ('o', eye),
                        (false, false) => ('-', fur),
                    };
                    font::draw_char(fb, x, y, s, col, glyph);
                } else if r == NOSE_ROW && chr == 'Y' {
                    font::draw_char(fb, x, y, s, nose, chr);
                } else {
                    font::draw_char(fb, x, y, s, fur, chr);
                }
            }
        }
        // Tongue: out while panting, flicking in and out a few times a second.
        if awake && self.pant > 0.0 && (self.t * 3.0).fract() < 0.6 {
            let (r, c) = TONGUE;
            font::draw_char(fb, ox + c as i32 * adv, oy + r as i32 * row_h, s, rgb(235, 115, 135), 'U');
        }
        let tail = TAILS[if awake { self.tail % WAG_POSES } else { REST_TAIL }];
        for (r, line) in tail.iter().enumerate() {
            let y = oy + (TAIL_ROW + r) as i32 * row_h;
            font::draw_text(fb, ox + TAIL_COL as i32 * adv, y, s, fur, line);
        }

        if barking {
            // Pops up beside the head and fades as the bark ends.
            let col = scale(rgb(225, 215, 170), (self.bark / 0.9).clamp(0.3, 1.0));
            font::draw_text(fb, ox + 12 * adv, oy - row_h - row_h / 2, s, col, BARK);
        }

        if self.mood == Some(Mood::Asleep) {
            // Three z's rising from the head, each fading out over a 3 s cycle.
            let head_x = ox + 12 * adv;
            for k in 0..3 {
                let p = (self.zzz / 3.0 + k as f32 / 3.0).fract();
                let col = scale(rgb(150, 160, 200), (1.0 - p).max(0.0));
                let y = oy - (p * 2.0 * row_h as f32) as i32;
                let x = head_x + (p * 2.0 * adv as f32) as i32 + k * adv / 2;
                font::draw_char(fb, x, y, s, col, if k == 2 { 'Z' } else { 'z' });
            }
        }

        if let Some(b) = self.ball {
            // The ball bounces along the ground line under the paws.
            let bw = font::text_width("o", s);
            let span = (w + bw) as f32;
            let x = if b.to_right { (b.pos * span) as i32 - bw } else { w - (b.pos * span) as i32 };
            let hop = (b.pos * BALL_BOUNCES * PI).sin().abs() * 2.5 * row_h as f32;
            let y = oy + ch - hop as i32;
            font::draw_char(fb, x, y, s, rgb(225, 95, 60), 'o');
        }
    }
    fn wants_fps(&self) -> u32 {
        15
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(w: u32, h: u32, n: usize, dt_ms: u64) -> Vec<Framebuffer> {
        let mut p = Puppy::default();
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
    fn art_is_rectangular() {
        assert!(BODY.iter().all(|l| l.chars().count() == BODY_W));
        assert!(TAILS.iter().flatten().all(|l| l.chars().count() == TAIL_W));
        assert_eq!(BODY[EYE_ROW].matches('o').count(), 2);
        assert_eq!(BODY[NOSE_ROW].matches('Y').count(), 1);
        assert_eq!(BODY[TONGUE.0].chars().nth(TONGUE.1), Some(' '));
    }

    #[test]
    fn budget_and_content() {
        // Two minutes at half-second steps: covers panting, barking, the ball and a nap.
        for fb in frames(1280, 800, 240, 500) {
            let lit = fb.lit_fraction();
            assert!(lit > 0.0 && lit <= 0.05, "lit {lit}");
        }
    }

    #[test]
    fn animates_over_time() {
        let f = frames(1280, 800, 240, 500);
        assert_ne!(f[0], f[239]);
        assert!(f.windows(2).filter(|p| p[0] != p[1]).count() > 50);
    }

    #[test]
    fn moods_and_antics_all_happen() {
        let mut p = Puppy::default();
        let mut rng = Rng::new(9);
        p.init(640, 480, 0.05, &mut rng);
        let (mut slept, mut woke, mut barked, mut panted, mut played) = (false, false, false, false, false);
        for _ in 0..3000 {
            p.advance(0.1, &mut rng);
            match p.mood {
                Some(Mood::Asleep) => {
                    slept = true;
                    assert_eq!(p.tail, REST_TAIL, "tail rests while asleep");
                }
                Some(Mood::Awake) => woke = true,
                None => {}
            }
            barked |= p.bark > 0.0;
            panted |= p.pant > 0.0;
            played |= p.ball.is_some();
        }
        assert!(slept && woke && barked && panted && played);
    }

    #[test]
    fn sizes() {
        for (w, h) in [(1, 1), (10, 10), (3840, 2160)] {
            assert_eq!(frames(w, h, 2, 100).len(), 2);
        }
    }
}
