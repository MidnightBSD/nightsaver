//! ASCII-art penguin pattern. An original little penguin drawn with the bitmap font: it
//! waddles back and forth, stops to flap its flippers and blink, perks up when a fish
//! leaps past, and dozes standing up with drifting z's, all under a few falling snowflakes.
//! Text is sparse by nature and nothing stays put, which keeps it OLED friendly.
use super::{Pattern, Rng};
use crate::render::font::{self, GLYPH_H, GLYPH_W};
use crate::render::{Framebuffer, rgb, scale};
use std::f32::consts::TAU;
use std::time::Duration;

const BODY_W: usize = 11;
/// Head, three rows. Row 1 holds the eyes (`o`), row 2 the beak (`V`, or `<` / `>` when
/// the penguin looks where it is walking).
const HEAD: [&str; 3] = [
    r"   .---.   ",
    r"  / o o \  ",
    r"  \  V  /  ",
];
const EYE_ROW: usize = 1;
const BEAK_ROW: usize = 2;
/// Body rows 3..5 with the flippers down, half raised and up.
const FLIPPERS: [[&str; 2]; 3] = [
    [r" /(     )\ ", r"/ (     ) \"],
    [r"--(     )--", r"  (     )  "],
    [r"\_(     )_/", r"  (     )  "],
];
const BELLY: &str = r"   \___/   ";
/// Feet: standing, then the two halves of a waddle step.
const FEET: [&str; 3] = [
    r"   _/ \_   ",
    r"   _/ \    ",
    r"    / \_   ",
];
const ROWS: usize = HEAD.len() + 2 + 2;

const FISH_RIGHT: &str = "><>";
const FISH_LEFT: &str = "<><";
/// Seconds a fish is in the air.
const FISH_AIRTIME: f32 = 2.6;
const FLAKES: usize = 14;
/// Vertical drift period in seconds.
const PERIOD_Y: f32 = 331.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mood {
    Walk,
    Stand,
    Nap,
}

#[derive(Debug, Clone, Copy)]
struct Fish {
    /// Progress through the leap, 0.0..1.0.
    pos: f32,
    /// Where the leap starts, as a fraction of the screen width.
    from: f32,
    to_right: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct Flake {
    /// Position as fractions of the screen.
    x: f32,
    y: f32,
    /// Fall speed in screen heights per second, and sway phase.
    speed: f32,
    sway: f32,
    big: bool,
}

#[derive(Debug, Default)]
pub struct Penguin {
    width: u32,
    height: u32,
    scale: u32,
    t: f32,
    phase: f32,
    /// Horizontal position of the figure, 0.0..1.0 of the free width.
    x: f32,
    to_right: bool,
    mood: Option<Mood>,
    /// Seconds until the mood changes.
    mood_left: f32,
    /// Seconds until the next blink; negative while the eyes are shut.
    blink: f32,
    /// Waddle step phase in seconds.
    step: f32,
    /// Seconds of flapping left and seconds until the next flap.
    flap: f32,
    flap_in: f32,
    fish: Option<Fish>,
    fish_in: f32,
    /// Phase of the floating z's while napping.
    zzz: f32,
    flakes: Vec<Flake>,
}

impl Penguin {
    fn set_mood(&mut self, mood: Mood, rng: &mut Rng) {
        self.mood = Some(mood);
        self.mood_left = match mood {
            Mood::Walk => 10.0 + 14.0 * rng.next_f32(),
            Mood::Stand => 5.0 + 7.0 * rng.next_f32(),
            Mood::Nap => 10.0 + 14.0 * rng.next_f32(),
        };
        self.blink = 2.0 + 4.0 * rng.next_f32();
        self.flap = 0.0;
        self.flap_in = 1.0 + 3.0 * rng.next_f32();
        self.zzz = 0.0;
        if mood == Mood::Walk && rng.next_u32().is_multiple_of(3) {
            self.to_right = !self.to_right;
        }
    }

    fn next_mood(mood: Mood, rng: &mut Rng) -> Mood {
        match mood {
            Mood::Walk => Mood::Stand,
            // After standing about: mostly walk on, sometimes nod off.
            Mood::Stand if rng.next_u32().is_multiple_of(3) => Mood::Nap,
            Mood::Stand | Mood::Nap => Mood::Walk,
        }
    }

    fn excited(&self) -> bool {
        self.fish.is_some() && self.mood != Some(Mood::Nap)
    }

    fn advance(&mut self, dts: f32, rng: &mut Rng) {
        let Some(mut mood) = self.mood else {
            self.set_mood(Mood::Stand, rng);
            return;
        };
        self.mood_left -= dts;
        if self.mood_left <= 0.0 {
            mood = Self::next_mood(mood, rng);
            self.set_mood(mood, rng);
        }
        match mood {
            Mood::Walk => {
                // About 50 s to cross the free width; turn round at either end.
                self.step += dts;
                self.x += if self.to_right { dts } else { -dts } / 50.0;
                if self.x >= 1.0 {
                    (self.x, self.to_right) = (1.0, false);
                } else if self.x <= 0.0 {
                    (self.x, self.to_right) = (0.0, true);
                }
            }
            Mood::Stand => {
                if self.flap > 0.0 {
                    self.flap -= dts;
                } else {
                    self.flap_in -= dts;
                    if self.flap_in <= 0.0 {
                        self.flap = 1.2 + 1.2 * rng.next_f32();
                        self.flap_in = 2.0 + 4.0 * rng.next_f32();
                    }
                }
            }
            Mood::Nap => self.zzz += dts,
        }
        if mood != Mood::Nap {
            // Blink: eyes shut for 0.15 s every few seconds.
            self.blink -= dts;
            if self.blink < -0.15 {
                self.blink = 2.0 + 5.0 * rng.next_f32();
            }
            self.fish_in -= dts;
            if self.fish.is_none() && self.fish_in <= 0.0 {
                self.fish = Some(Fish {
                    pos: 0.0,
                    from: 0.1 + 0.6 * rng.next_f32(),
                    to_right: rng.next_u32().is_multiple_of(2),
                });
                self.fish_in = 18.0 + 30.0 * rng.next_f32();
            }
        }
        if let Some(f) = &mut self.fish {
            f.pos += dts / FISH_AIRTIME;
            if f.pos >= 1.0 {
                self.fish = None;
            }
        }
        for f in &mut self.flakes {
            f.y += f.speed * dts;
            if f.y >= 1.0 {
                *f = Self::flake(rng, false);
            }
        }
    }

    fn flake(rng: &mut Rng, anywhere: bool) -> Flake {
        Flake {
            x: rng.next_f32(),
            y: if anywhere { rng.next_f32() } else { 0.0 },
            speed: 0.02 + 0.03 * rng.next_f32(),
            sway: rng.next_f32() * TAU,
            big: rng.next_u32().is_multiple_of(3),
        }
    }

    /// Flipper pose: flapping while it stands (faster when a fish is about), a gentle
    /// swing in step while it walks, down otherwise.
    fn flipper_pose(&self) -> usize {
        match self.mood {
            Some(Mood::Stand) if self.flap > 0.0 || self.excited() => {
                [0, 1, 2, 1][(self.t * 8.0) as usize % 4]
            }
            Some(Mood::Walk) => usize::from((self.step * 2.0) as usize % 4 == 3),
            _ => 0,
        }
    }

    fn feet_pose(&self) -> usize {
        if self.mood == Some(Mood::Walk) { 1 + (self.step * 2.0) as usize % 2 } else { 0 }
    }
}

impl Pattern for Penguin {
    fn name(&self) -> &'static str {
        "penguin"
    }
    fn init(&mut self, width: u32, height: u32, _lit_budget: f32, rng: &mut Rng) {
        self.width = width;
        self.height = height;
        self.t = 0.0;
        self.phase = rng.next_f32() * TAU;
        // Penguin about a fifth of the screen width: it needs room to waddle.
        let unit = font::text_width("x".repeat(BODY_W).as_str(), 1).max(1) as u32;
        self.scale = (width / 5 / unit).max(1);
        self.x = rng.next_f32();
        self.to_right = rng.next_u32().is_multiple_of(2);
        self.mood = None;
        self.step = 0.0;
        self.fish = None;
        self.fish_in = 6.0 + 15.0 * rng.next_f32();
        self.flakes = (0..FLAKES).map(|_| Self::flake(rng, true)).collect();
        self.advance(0.0, rng);
    }
    fn step(&mut self, dt: Duration, fb: &mut Framebuffer, rng: &mut Rng) {
        let dts = dt.as_secs_f32().min(0.5);
        self.t += dts;
        self.advance(dts, rng);

        let s = self.scale.max(1);
        let adv = ((GLYPH_W + 1) * s) as i32;
        let row_h = ((GLYPH_H + 1) * s) as i32;
        let cw = BODY_W as i32 * adv;
        let ch = ROWS as i32 * row_h;
        // Leave two rows above for the z's and one below for the fish.
        let total_h = ch + 3 * row_h;
        let (w, h) = (fb.width as i32, fb.height as i32);

        // Snow first, so the penguin is drawn over it.
        let snow = rgb(120, 130, 155);
        for f in &self.flakes {
            let x = (f.x * w as f32 + (self.t * 0.4 + f.sway).sin() * adv as f32) as i32;
            let y = (f.y * h as f32) as i32;
            let fs = if f.big { (s / 2).max(1) } else { (s / 3).max(1) };
            font::draw_char(fb, x, y, fs, snow, if f.big { '*' } else { '.' });
        }

        let walking = self.mood == Some(Mood::Walk);
        let napping = self.mood == Some(Mood::Nap);
        // A waddle rocks the whole bird a little from side to side with each step.
        let sway = if walking { ((self.step * TAU).sin() * 0.3 * adv as f32) as i32 } else { 0 };
        let ay = ((h - total_h).max(0) as f32) / 2.0;
        let ox = (self.x * (w - cw).max(0) as f32) as i32 + sway;
        let oy = (ay + ay * (self.t / PERIOD_Y * TAU + self.phase).sin()) as i32 + 2 * row_h;

        // Clear the bird's box so no flake shows through it.
        fb.fill_rect(ox, oy, cw as u32, ch as u32, 0);
        let coat = rgb(140, 160, 195);
        let eye = rgb(225, 232, 240);
        let orange = rgb(240, 160, 60);
        let open = !napping && self.blink >= 0.0;
        let happy = self.excited();
        let beak = match (walking, self.to_right) {
            (true, true) => '>',
            (true, false) => '<',
            _ => 'V',
        };
        let flippers = FLIPPERS[self.flipper_pose()];
        let rows = [HEAD[0], HEAD[1], HEAD[2], flippers[0], flippers[1], BELLY, FEET[self.feet_pose()]];
        for (r, line) in rows.iter().enumerate() {
            let y = oy + r as i32 * row_h;
            for (c, chr) in line.chars().enumerate() {
                if chr == ' ' {
                    continue;
                }
                let x = ox + c as i32 * adv;
                let (glyph, col) = if r == EYE_ROW && chr == 'o' {
                    // Bright-eyed for a fish, shut while blinking or napping.
                    match (happy, open) {
                        (true, _) => ('^', eye),
                        (false, true) => ('o', eye),
                        (false, false) => ('-', coat),
                    }
                } else if r == BEAK_ROW && chr == 'V' {
                    (beak, orange)
                } else if r == rows.len() - 1 {
                    (chr, orange)
                } else {
                    (chr, coat)
                };
                font::draw_char(fb, x, y, s, col, glyph);
            }
        }

        if napping {
            // Three z's rising from the head, each fading out over a 3 s cycle.
            let head_x = ox + 8 * adv;
            for k in 0..3 {
                let p = (self.zzz / 3.0 + k as f32 / 3.0).fract();
                let col = scale(rgb(150, 160, 200), (1.0 - p).max(0.0));
                let y = oy - (p * 2.0 * row_h as f32) as i32;
                let x = head_x + (p * 2.0 * adv as f32) as i32 + k * adv / 2;
                font::draw_char(fb, x, y, s, col, if k == 2 { 'Z' } else { 'z' });
            }
        }

        if let Some(f) = self.fish {
            // A leap: up and over on a parabola from below the feet, drifting sideways.
            let text = if f.to_right { FISH_RIGHT } else { FISH_LEFT };
            let reach = 0.2 * w as f32;
            let dx = if f.to_right { f.pos * reach } else { -f.pos * reach };
            let x = (f.from * w as f32 + dx) as i32;
            let hop = 4.0 * f.pos * (1.0 - f.pos) * 3.0 * row_h as f32;
            let y = oy + ch + row_h - hop as i32;
            font::draw_text(fb, x, y, s, rgb(110, 190, 225), text);
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
        let mut p = Penguin::default();
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
        let rows = HEAD.iter().chain(FLIPPERS.iter().flatten()).chain(FEET.iter()).chain([&BELLY]);
        for l in rows {
            assert_eq!(l.chars().count(), BODY_W, "{l:?}");
        }
        assert_eq!(HEAD[EYE_ROW].matches('o').count(), 2);
        assert_eq!(HEAD[BEAK_ROW].matches('V').count(), 1);
    }

    #[test]
    fn budget_and_content() {
        // Two minutes at half-second steps: covers walking, flapping, a fish and a nap.
        for fb in frames(1280, 800, 240, 500) {
            let lit = fb.lit_fraction();
            assert!(lit > 0.0 && lit <= 0.05, "lit {lit}");
        }
    }

    #[test]
    fn animates_over_time() {
        let f = frames(1280, 800, 240, 500);
        assert_ne!(f[0], f[239]);
        assert!(f.windows(2).filter(|p| p[0] != p[1]).count() > 200);
    }

    #[test]
    fn moods_and_antics_all_happen() {
        let mut p = Penguin::default();
        let mut rng = Rng::new(9);
        p.init(640, 480, 0.05, &mut rng);
        let (mut walked, mut stood, mut napped, mut flapped, mut fished) = (false, false, false, false, false);
        let (mut lo, mut hi) = (p.x, p.x);
        for _ in 0..6000 {
            p.advance(0.1, &mut rng);
            match p.mood {
                Some(Mood::Walk) => walked = true,
                Some(Mood::Stand) => stood = true,
                Some(Mood::Nap) => napped = true,
                None => {}
            }
            flapped |= p.flap > 0.0;
            fished |= p.fish.is_some();
            assert!((0.0..=1.0).contains(&p.x), "walked off the screen: {}", p.x);
            (lo, hi) = (lo.min(p.x), hi.max(p.x));
        }
        assert!(walked && stood && napped && flapped && fished);
        assert!(hi - lo > 0.3, "the penguin should get about: {lo}..{hi}");
    }

    #[test]
    fn sizes() {
        for (w, h) in [(1, 1), (10, 10), (3840, 2160)] {
            assert_eq!(frames(w, h, 2, 100).len(), 2);
        }
    }
}
