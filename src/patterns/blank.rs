//! Blank pattern. Owner: Sonnet D.
use super::{Pattern, Rng};
use crate::render::Framebuffer;
use std::time::Duration;

#[derive(Debug, Default)]
pub struct Blank {}

impl Pattern for Blank {
    fn name(&self) -> &'static str {
        "blank"
    }
    fn init(&mut self, _width: u32, _height: u32, _lit_budget: f32, _rng: &mut Rng) {}
    fn step(&mut self, _dt: Duration, _fb: &mut Framebuffer, _rng: &mut Rng) {}
    fn wants_fps(&self) -> u32 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_nothing() {
        let mut p = Blank::default();
        let mut rng = Rng::new(1);
        p.init(64, 64, 0.05, &mut rng);
        let mut fb = Framebuffer::new(64, 64);
        p.step(Duration::from_millis(100), &mut fb, &mut rng);
        assert!(fb.pixels.iter().all(|&p| p == 0));
        assert_eq!(p.wants_fps(), 0);
    }
}
