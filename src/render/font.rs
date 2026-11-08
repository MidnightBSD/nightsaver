//! Tiny built-in bitmap font (5x7): digits, ':', '.', '-', '/', A-Z, a-z, space. Owner: Sonnet D.

use super::Framebuffer;

pub const GLYPH_W: u32 = 5;
pub const GLYPH_H: u32 = 7;

const BLANK: [u8; 7] = [0; 7];

/// Rows of 5 bits each (bit 4 = leftmost pixel).
fn glyph(c: char) -> &'static [u8; 7] {
    match c.to_ascii_uppercase() {
        '0' => &[0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        '1' => &[0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => &[0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
        '3' => &[0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110],
        '4' => &[0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => &[0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        '6' => &[0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => &[0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => &[0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => &[0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
        ':' => &[0b00000, 0b00100, 0b00100, 0b00000, 0b00100, 0b00100, 0b00000],
        '.' => &[0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b01100, 0b01100],
        '-' => &[0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000],
        '/' => &[0b00001, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b10000],
        'A' => &[0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'B' => &[0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
        'C' => &[0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110],
        'D' => &[0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110],
        'E' => &[0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
        'F' => &[0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
        'G' => &[0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111],
        'H' => &[0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'I' => &[0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        'J' => &[0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100],
        'K' => &[0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001],
        'L' => &[0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111],
        'M' => &[0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
        'N' => &[0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001],
        'O' => &[0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'P' => &[0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
        'Q' => &[0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101],
        'R' => &[0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
        'S' => &[0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
        'T' => &[0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
        'U' => &[0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'V' => &[0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
        'W' => &[0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b10101, 0b01010],
        'X' => &[0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001],
        'Y' => &[0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100],
        'Z' => &[0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111],
        _ => &BLANK,
    }
}

/// Draw one character with its top-left at (x,y) at integer `scale` (min 1).
pub fn draw_char(fb: &mut Framebuffer, x: i32, y: i32, scale: u32, rgb: u32, c: char) {
    let s = scale.max(1);
    let g = glyph(c);
    for (row, bits) in g.iter().enumerate() {
        for col in 0..GLYPH_W {
            if bits & (1 << (GLYPH_W - 1 - col)) != 0 {
                fb.fill_rect(x + (col * s) as i32, y + (row as u32 * s) as i32, s, s, rgb);
            }
        }
    }
}

/// Draw `text` at (x,y) with integer `scale`; returns width drawn in pixels.
pub fn draw_text(fb: &mut Framebuffer, x: i32, y: i32, scale: u32, rgb: u32, text: &str) -> i32 {
    let s = scale.max(1);
    let advance = ((GLYPH_W + 1) * s) as i32;
    let mut cx = x;
    for c in text.chars() {
        draw_char(fb, cx, y, s, rgb, c);
        cx += advance;
    }
    text_width(text, s)
}

/// Width in pixels of `text` at `scale` (1 scaled pixel of spacing between glyphs).
pub fn text_width(text: &str, scale: u32) -> i32 {
    let s = scale.max(1) as i32;
    let n = text.chars().count() as i32;
    if n == 0 { 0 } else { n * GLYPH_W as i32 * s + (n - 1) * s }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_math() {
        assert_eq!(text_width("", 3), 0);
        assert_eq!(text_width("1", 1), 5);
        assert_eq!(text_width("12:34", 1), 29);
        assert_eq!(text_width("12:34", 3), 87);
    }

    #[test]
    fn draws_inside_box() {
        let mut fb = Framebuffer::new(200, 100);
        let (x0, y0, s) = (20, 30, 3u32);
        let w = draw_text(&mut fb, x0, y0, s, 0xffffff, "12:34");
        assert_eq!(w, 87);
        let h = (GLYPH_H * s) as i32;
        let mut lit = 0;
        for y in 0..100 {
            for x in 0..200 {
                if fb.get(x, y).unwrap() != 0 {
                    lit += 1;
                    assert!(x >= x0 && x < x0 + w && y >= y0 && y < y0 + h, "pixel {x},{y}");
                }
            }
        }
        assert!(lit > 100);
    }

    #[test]
    fn lowercase_and_unknown() {
        let mut a = Framebuffer::new(20, 10);
        let mut b = a.clone();
        draw_text(&mut a, 0, 0, 1, 0xffffff, "a");
        draw_text(&mut b, 0, 0, 1, 0xffffff, "A");
        assert_eq!(a, b);
        let mut c = Framebuffer::new(20, 10);
        draw_text(&mut c, 0, 0, 1, 0xffffff, " ");
        assert_eq!(c.lit_fraction(), 0.0);
    }

    #[test]
    fn offscreen_is_clipped() {
        let mut fb = Framebuffer::new(4, 4);
        draw_text(&mut fb, -50, -50, 2, 0xffffff, "88");
        draw_text(&mut fb, 100, 100, 2, 0xffffff, "88");
    }
}
