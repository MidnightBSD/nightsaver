//! Dim unlock dialog rendered into the framebuffer. Owner: Opus E.
//! Contract: `fn draw(fb: &mut Framebuffer, locker: &Locker, now: Instant)`.
//!
//! OLED: every colour has max channel <= 150, the panel is mostly black, and its position
//! drifts by a few pixels over time so no element sits on the same pixels for long.
//! The bitmap font has no `*` glyph, so the mask marks are drawn by `draw_mark`.

use std::sync::OnceLock;
use std::time::Instant;

use super::{LockState, Locker};
use crate::render::Framebuffer;
use crate::render::font::{self, GLYPH_H};

const BORDER: u32 = 0x00_3c_3c_3c;
const FILL: u32 = 0x00_00_00_00;
const USER: u32 = 0x00_80_80_80;
const MASK: u32 = 0x00_96_96_96;
const STATUS: u32 = 0x00_6e_6e_6e;
const ERROR: u32 = 0x00_96_28_28;

const PANEL_W: u32 = 300;
const PANEL_H: u32 = 110;
/// Maximum drift from the centred position, in pixels.
const DRIFT_X: f32 = 6.0;
const DRIFT_Y: f32 = 4.0;

/// 5x5 asterisk used for each entered character.
const MARK: [u8; 5] = [0b00100, 0b10101, 0b01110, 0b10101, 0b00100];

fn epoch() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

/// Font scale for a framebuffer of this size.
fn scale_for(fb_w: u32, fb_h: u32) -> u32 {
    if fb_w >= PANEL_W + 20 && fb_h >= PANEL_H + 20 { 2 } else { 1 }
}

/// Panel rectangle `(x, y, w, h)` for a `fb_w` x `fb_h` framebuffer at `now`: centred, drifting
/// a few pixels with time, always fully inside the framebuffer.
pub fn panel_rect(fb_w: u32, fb_h: u32, now: Instant) -> (i32, i32, u32, u32) {
    let s = scale_for(fb_w, fb_h);
    let w = (PANEL_W * s / 2).min(fb_w);
    let h = (PANEL_H * s / 2).min(fb_h);
    let t = now.saturating_duration_since(epoch()).as_secs_f32();
    let dx = (DRIFT_X * (t / 23.0).sin()).round() as i32;
    let dy = (DRIFT_Y * (t / 31.0 + 1.0).sin()).round() as i32;
    let max_x = (fb_w - w) as i32;
    let max_y = (fb_h - h) as i32;
    let x = (max_x / 2 + dx).clamp(0, max_x);
    let y = (max_y / 2 + dy).clamp(0, max_y);
    (x, y, w, h)
}

fn draw_mark(fb: &mut Framebuffer, x: i32, y: i32, s: u32, rgb: u32) {
    for (row, bits) in MARK.iter().enumerate() {
        for col in 0..5u32 {
            if bits & (1 << (4 - col)) != 0 {
                fb.fill_rect(x + (col * s) as i32, y + (row as u32 * s) as i32, s, s, rgb);
            }
        }
    }
}

/// Draw `text` horizontally centred in `[x, x+w)`, truncated to fit.
fn draw_centered(fb: &mut Framebuffer, x: i32, w: u32, y: i32, s: u32, rgb: u32, text: &str) {
    let mut t: String = text.to_string();
    while !t.is_empty() && font::text_width(&t, s) > w as i32 {
        t.pop();
    }
    let tw = font::text_width(&t, s);
    font::draw_text(fb, x + (w as i32 - tw) / 2, y, s, rgb, &t);
}

/// Draw the unlock dialog on top of `fb` when it is visible (Prompting, Checking, Failed).
pub fn draw(fb: &mut Framebuffer, locker: &Locker, now: Instant) {
    if !locker.is_prompt_visible() || fb.width < 8 || fb.height < 8 {
        return;
    }
    let s = scale_for(fb.width, fb.height);
    let (px, py, pw, ph) = panel_rect(fb.width, fb.height, now);
    fb.fill_rect(px, py, pw, ph, FILL);
    // 1-scaled-pixel border.
    fb.fill_rect(px, py, pw, s, BORDER);
    fb.fill_rect(px, py + (ph - s) as i32, pw, s, BORDER);
    fb.fill_rect(px, py, s, ph, BORDER);
    fb.fill_rect(px + (pw - s) as i32, py, s, ph, BORDER);

    let pad = 6 * s;
    let inner_x = px + pad as i32;
    let inner_w = pw.saturating_sub(2 * pad);
    let line = (GLYPH_H * s) as i32;
    let gap = ((ph as i32) - 3 * line) / 4;

    // Username.
    let y1 = py + gap;
    draw_centered(fb, inner_x, inner_w, y1, s, USER, &locker.user);

    // Mask row.
    let y2 = y1 + line + gap;
    let advance = (6 * s) as i32;
    let max_marks = ((inner_w as i32 + s as i32) / advance).max(0) as usize;
    let n = locker.masked_len().min(max_marks);
    if n > 0 {
        let row_w = n as i32 * advance - s as i32;
        let mut mx = inner_x + (inner_w as i32 - row_w) / 2;
        let my = y2 + s as i32; // 5-row mark centred in the 7-row line
        for _ in 0..n {
            draw_mark(fb, mx, my, s, MASK);
            mx += advance;
        }
    }

    // Status line.
    let y3 = y2 + line + gap;
    let (text, color) = status_line(locker);
    // Long PAM messages: drop to the smaller font before truncating.
    let ss = if s > 1 && font::text_width(text, s) > inner_w as i32 { s - 1 } else { s };
    draw_centered(fb, inner_x, inner_w, y3, ss, color, text);
}

/// Status line text and colour. Only fixed strings or the sanitised PAM message from
/// `Locker::failure_text` (which never contains the password) are shown.
fn status_line(locker: &Locker) -> (&str, u32) {
    match locker.state {
        LockState::Checking { .. } if locker.is_auth_busy() => ("PAM busy, please wait", STATUS),
        LockState::Checking { .. } => ("Checking...", STATUS),
        LockState::Failed { .. } if locker.backoff_notice() => ("Please wait...", STATUS),
        LockState::Failed { .. } => (locker.failure_text().unwrap_or("Incorrect password"), ERROR),
        _ => ("Enter password", STATUS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::{AuthFn, Locker};
    use crate::input::KeyInput;
    use std::sync::Arc;
    use std::time::Duration;

    fn locker() -> Locker {
        let auth: AuthFn = Arc::new(|_: &str, _: &str, _: &zeroize::Zeroizing<String>| Ok(()));
        Locker::with_auth("laffer1".into(), "svc".into(), Duration::from_secs(30), auth)
    }

    fn max_channel(p: u32) -> u32 {
        ((p >> 16) & 0xff).max((p >> 8) & 0xff).max(p & 0xff)
    }

    fn assert_inside(fb: &Framebuffer, now: Instant) -> usize {
        let (x, y, w, h) = panel_rect(fb.width, fb.height, now);
        let mut lit = 0;
        for yy in 0..fb.height as i32 {
            for xx in 0..fb.width as i32 {
                let p = fb.get(xx, yy).unwrap();
                if p != 0 {
                    lit += 1;
                    assert!(xx >= x && xx < x + w as i32 && yy >= y && yy < y + h as i32, "lit {xx},{yy}");
                    assert!(max_channel(p) <= 150, "too bright {p:06x}");
                }
            }
        }
        lit
    }

    #[test]
    fn hidden_draws_nothing() {
        let l = locker();
        let mut fb = Framebuffer::new(640, 480);
        draw(&mut fb, &l, Instant::now());
        assert!(fb.pixels.iter().all(|&p| p == 0));
    }

    #[test]
    fn prompting_stays_inside_panel_and_dim() {
        let mut l = locker();
        let now = Instant::now();
        l.key(KeyInput::Other, now);
        let mut fb = Framebuffer::new(640, 480);
        draw(&mut fb, &l, now);
        let base = assert_inside(&fb, now);
        assert!(base > 0);
        for c in "pass".chars() {
            l.key(KeyInput::Text(c.to_string()), now);
        }
        let mut fb2 = Framebuffer::new(640, 480);
        draw(&mut fb2, &l, now);
        assert!(assert_inside(&fb2, now) > base, "mask marks drawn");
        // Tiny framebuffer: still clipped to the panel and no panic.
        let mut small = Framebuffer::new(100, 40);
        draw(&mut small, &l, now);
        assert_inside(&small, now);
    }

    #[test]
    fn failed_is_drawn_and_panel_covers_pattern() {
        let mut l = locker();
        let now = Instant::now();
        l.state = LockState::Failed { until: now + Duration::from_secs(2), retry_after: now };
        let mut fb = Framebuffer::new(640, 480);
        fb.pixels.fill(0x00_ff_ff_ff);
        draw(&mut fb, &l, now);
        let (x, y, w, h) = panel_rect(640, 480, now);
        let cx = x + w as i32 / 2;
        let cy = y + h as i32 / 2;
        assert!(max_channel(fb.get(cx, cy).unwrap()) <= 150);
    }

    #[test]
    fn status_texts() {
        let mut l = locker();
        let now = Instant::now();
        assert_eq!(status_line(&l).0, "Enter password");
        l.state = LockState::Failed { until: now + Duration::from_secs(2), retry_after: now + Duration::from_secs(4) };
        assert_eq!(status_line(&l).0, "Incorrect password");
        // A key during the backoff switches to "Please wait..." and is not recorded.
        l.key(KeyInput::Text("x".into()), now);
        assert_eq!(status_line(&l).0, "Please wait...");
        assert_eq!(l.masked_len(), 0);
        let mut fb = Framebuffer::new(640, 480);
        draw(&mut fb, &l, now);
        assert_inside(&fb, now);

        // A PAM message is shown instead of the generic text (drawn smaller if long).
        let auth: AuthFn = Arc::new(|_: &str, _: &str, _: &zeroize::Zeroizing<String>| {
            Err(crate::lock::pam::PamError::Denied(format!(
                "authentication error{}Your password has expired, contact your administrator",
                crate::lock::pam::PAM_MESSAGE_SEP
            )))
        });
        let mut l = Locker::with_auth("laffer1".into(), "svc".into(), Duration::from_secs(30), auth);
        l.key(KeyInput::Other, now);
        l.key(KeyInput::Text("p".into()), now);
        l.key(KeyInput::Enter, now);
        assert_eq!(status_line(&l).0, "Checking...");
        for _ in 0..400 {
            if l.poll(now) != crate::lock::LockEvent::None {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(status_line(&l).0, "Your password has expired, contact your administrator");
        let mut fb = Framebuffer::new(640, 480);
        draw(&mut fb, &l, now);
        assert_inside(&fb, now);
    }

    #[test]
    fn panel_drifts_but_stays_in_bounds() {
        let base = epoch();
        let mut seen = std::collections::HashSet::new();
        for secs in (0..600).step_by(10) {
            let r = panel_rect(1280, 800, base + Duration::from_secs(secs));
            assert!(r.0 >= 0 && r.1 >= 0 && r.0 + r.2 as i32 <= 1280 && r.1 + r.3 as i32 <= 800);
            seen.insert((r.0, r.1));
        }
        assert!(seen.len() > 3, "panel should move over time");
    }
}
