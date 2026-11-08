//! Debug helper: render one frame of a pattern to a PPM file.
//! Usage: cargo run --example pattern_preview -- <name> <out.ppm> [width height] [frames]
use nightsaver::patterns::{Rng, by_name};
use nightsaver::render::Framebuffer;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let name = args.get(1).map(String::as_str).unwrap_or("midnight");
    let out = args.get(2).map(String::as_str).unwrap_or("frame.ppm");
    let w: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1280);
    let h: u32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(800);
    let frames: usize = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(60);
    let mut p = by_name(name).expect("unknown pattern");
    let mut rng = Rng::new(7);
    p.init(w, h, 0.05, &mut rng);
    let mut fb = Framebuffer::new(w, h);
    for _ in 0..frames {
        fb = Framebuffer::new(w, h);
        p.step(Duration::from_millis(100), &mut fb, &mut rng);
    }
    println!("{name}: lit fraction {:.4}", fb.lit_fraction());
    let mut data = format!("P6\n{w} {h}\n255\n").into_bytes();
    for px in &fb.pixels {
        data.extend([(px >> 16) as u8, (px >> 8) as u8, *px as u8]);
    }
    std::fs::write(out, data).expect("write ppm");
}
