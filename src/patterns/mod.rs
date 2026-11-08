//! Built-in patterns. Owner: Sonnet D.
pub mod blank;
pub mod clock;
pub mod matrix;
pub mod midnight;
pub mod starfield;

use crate::render::Framebuffer;
use std::time::Duration;

/// Simple deterministic PRNG so patterns need no external crate.
#[derive(Debug, Clone)]
pub struct Rng(u64);
impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    pub fn next_u32(&mut self) -> u32 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u32
    }
    pub fn next_f32(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 / (1u32 << 24) as f32
    }
    pub fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + (self.next_u32() % (hi - lo).max(1) as u32) as i32
    }
}

pub trait Pattern: Send {
    fn name(&self) -> &'static str;
    /// Called once per (re)size with the lit-pixel budget hint (0.0..=1.0).
    fn init(&mut self, width: u32, height: u32, lit_budget: f32, rng: &mut Rng);
    /// Advance by `dt` and draw a full frame into `fb` (fb is pre-cleared to black).
    fn step(&mut self, dt: Duration, fb: &mut Framebuffer, rng: &mut Rng);
    /// Desired frame rate; 0 means draw once and then never again.
    fn wants_fps(&self) -> u32 {
        30
    }
}

/// All pattern names, in canonical order (used for xscreensaver `-select N`, 1-based).
pub const NAMES: &[&str] = &["blank", "clock", "starfield", "matrix", "midnight"];

/// Construct a pattern by name. Unknown names (e.g. xscreensaver hack names in
/// `programs:`) map to `None`; callers fall back to "clock".
pub fn by_name(name: &str) -> Option<Box<dyn Pattern>> {
    match name.trim().to_ascii_lowercase().as_str() {
        "blank" => Some(Box::new(blank::Blank::default())),
        "clock" => Some(Box::new(clock::Clock::default())),
        "starfield" | "stars" => Some(Box::new(starfield::Starfield::default())),
        "matrix" | "glmatrix" | "xmatrix" => Some(Box::new(matrix::Matrix::default())),
        "midnight" | "midnightbsd" | "logo" => Some(Box::new(midnight::Midnight::default())),
        _ => None,
    }
}

/// Number of pixels the lit-budget hint allows for a `width` x `height` frame.
pub fn lit_budget_pixels(width: u32, height: u32, lit_budget: f32) -> usize {
    let px = width as f64 * height as f64;
    (px * f64::from(lit_budget.clamp(0.0, 1.0))) as usize
}
