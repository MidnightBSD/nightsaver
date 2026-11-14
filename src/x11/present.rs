//! Push a Framebuffer to a window via x11rb::image::Image::put (MIT-SHM later). Owner: Opus A.
//! Contract: `Presenter::new(&X11, width, height)`, `.ensure_size(&X11, w, h)`,
//! `.present(&X11, window: Window, &Framebuffer)`, `.destroy(&X11)`.
//!
//! The image is allocated in the server's native format for the root depth, so `Image::put`
//! uploads it without re-conversion (and splits it into several PutImage requests if it exceeds
//! the maximum request size). Fast path: 32 bpp, depth 24, RGB masks 0xff0000/0x00ff00/0x0000ff
//! (either byte order) — straight row copies. Anything else goes through a mask-based
//! conversion and `Image::put_pixel`.
//!
//! The framebuffer is drawn at (0,0) of `window`; only the overlap of the framebuffer and the
//! presenter's image size is copied, the rest of the image stays black.

use x11rb::connection::{Connection, RequestConnection};
use x11rb::image::{BitsPerPixel, Image, ImageOrder};
use x11rb::protocol::xproto::{ConnectionExt as _, CreateGCAux, Gcontext, Window};

use super::X11Error;
use super::conn::X11;
use crate::render::Framebuffer;

/// RGB channel masks of the root visual.
#[derive(Debug, Clone, Copy)]
struct Masks {
    r: u32,
    g: u32,
    b: u32,
}

impl Masks {
    fn of_root_visual(x: &X11) -> Masks {
        for d in &x.screen().allowed_depths {
            for v in &d.visuals {
                if v.visual_id == x.visual {
                    return Masks {
                        r: v.red_mask,
                        g: v.green_mask,
                        b: v.blue_mask,
                    };
                }
            }
        }
        log::warn!(
            "root visual {:#x} not found in setup; assuming 0xRRGGBB",
            x.visual
        );
        Masks {
            r: 0xff_0000,
            g: 0xff00,
            b: 0xff,
        }
    }

    fn is_rgb888(&self) -> bool {
        self.r == 0xff_0000 && self.g == 0xff00 && self.b == 0xff
    }

    /// Convert 0x00RRGGBB to a pixel value for this visual.
    fn convert(&self, rgb: u32) -> u32 {
        chan((rgb >> 16) & 0xff, self.r)
            | chan((rgb >> 8) & 0xff, self.g)
            | chan(rgb & 0xff, self.b)
    }
}

/// Place an 8-bit channel value into `mask` (scaled to the mask's width).
fn chan(v: u32, mask: u32) -> u32 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let bits = (mask >> shift).count_ones();
    let scaled = if bits <= 8 {
        v >> (8 - bits)
    } else {
        v << (bits - 8)
    };
    (scaled << shift) & mask
}

/// Owns a GC and a native-format client-side image.
pub struct Presenter {
    gc: Gcontext,
    image: Image<'static>,
    masks: Masks,
}

impl std::fmt::Debug for Presenter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Presenter")
            .field("gc", &self.gc)
            .field("width", &self.image.width())
            .field("height", &self.image.height())
            .finish()
    }
}

/// Size of the PutImage request header x11rb puts in front of the pixel data.
const PUT_IMAGE_HEADER: usize = 24;

/// True if a PutImage request of at most `max_request_bytes` can carry at least one image
/// line of `stride` bytes, i.e. `Image::put` can make progress.
fn fits_put_image(max_request_bytes: usize, stride: usize) -> bool {
    max_request_bytes.checked_sub(PUT_IMAGE_HEADER).is_some_and(|room| room >= stride.max(1))
}

fn alloc_image(x: &X11, width: u32, height: u32) -> Result<Image<'static>, X11Error> {
    let w = u16::try_from(width.max(1))
        .map_err(|_| X11Error::Protocol(format!("image width {width} too large")))?;
    let h = u16::try_from(height.max(1))
        .map_err(|_| X11Error::Protocol(format!("image height {height} too large")))?;
    Image::allocate_native(w, h, x.depth, x.conn.setup())
        .map_err(|e| X11Error::Protocol(format!("cannot allocate image: {e}")))
}

impl Presenter {
    pub fn new(x: &X11, width: u32, height: u32) -> Result<Presenter, X11Error> {
        let image = alloc_image(x, width, height)?;
        let gc = x.conn.generate_id()?;
        x.conn
            .create_gc(
                gc,
                x.root,
                &CreateGCAux::new().graphics_exposures(0).foreground(0),
            )?
            .check()?;
        Ok(Presenter {
            gc,
            image,
            masks: Masks::of_root_visual(x),
        })
    }

    pub fn width(&self) -> u32 {
        u32::from(self.image.width())
    }

    pub fn height(&self) -> u32 {
        u32::from(self.image.height())
    }

    /// Reallocate the image if the size changed.
    pub fn ensure_size(&mut self, x: &X11, width: u32, height: u32) -> Result<(), X11Error> {
        if self.width() != width.max(1) || self.height() != height.max(1) {
            self.image = alloc_image(x, width, height)?;
        }
        Ok(())
    }

    /// Convert `fb` into the image and upload it to `window` at (0,0), then flush.
    pub fn present(&mut self, x: &X11, window: Window, fb: &Framebuffer) -> Result<(), X11Error> {
        // `Image::put` computes `maximum_request_bytes - 24` and divides by the lines that
        // fit, both unchecked. A dead connection reports a maximum of 0, which panics there
        // (arithmetic overflow) instead of returning an error; report it as one ourselves.
        let stride = self.image.data().len() / usize::from(self.image.height()).max(1);
        let max = x.conn.maximum_request_bytes();
        if !fits_put_image(max, stride) {
            return Err(X11Error::Protocol(format!(
                "cannot upload a frame: the server accepts requests of {max} bytes (connection lost?)"
            )));
        }
        self.convert(fb);
        self.image.put(&x.conn, window, self.gc, 0, 0)?;
        x.flush();
        Ok(())
    }

    /// Free the GC (best effort).
    pub fn destroy(self, x: &X11) {
        let _ = x.conn.free_gc(self.gc);
        x.flush();
    }

    fn convert(&mut self, fb: &Framebuffer) {
        let iw = usize::from(self.image.width());
        let ih = usize::from(self.image.height());
        let fw = fb.width as usize;
        let w = iw.min(fw);
        let h = ih.min(fb.height as usize);
        let fast = self.image.bits_per_pixel() == BitsPerPixel::B32
            && self.image.depth() == 24
            && self.masks.is_rgb888();
        if fast {
            let big = self.image.byte_order() == ImageOrder::MsbFirst;
            let data = self.image.data_mut();
            let stride = data.len() / ih;
            for y in 0..ih {
                let row = &mut data[y * stride..y * stride + iw * 4];
                if y >= h {
                    row.fill(0);
                    continue;
                }
                let src = &fb.pixels[y * fw..y * fw + w];
                let (lit, rest) = row.split_at_mut(w * 4);
                for (dst, &p) in lit.chunks_exact_mut(4).zip(src) {
                    let p = p & 0x00ff_ffff;
                    dst.copy_from_slice(&if big {
                        p.to_be_bytes()
                    } else {
                        p.to_le_bytes()
                    });
                }
                rest.fill(0);
            }
        } else {
            let masks = self.masks;
            let black = masks.convert(0);
            for y in 0..ih {
                for xx in 0..iw {
                    let px = if y < h && xx < w {
                        masks.convert(fb.pixels[y * fw + xx])
                    } else {
                        black
                    };
                    self.image.put_pixel(xx as u16, y as u16, px);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_image_needs_room_for_one_line() {
        // A dead connection reports 0: must be refused, not subtracted from.
        assert!(!fits_put_image(0, 5120));
        assert!(!fits_put_image(PUT_IMAGE_HEADER, 5120));
        assert!(!fits_put_image(PUT_IMAGE_HEADER + 5119, 5120), "less than one line");
        assert!(fits_put_image(PUT_IMAGE_HEADER + 5120, 5120));
        assert!(fits_put_image(262_140, 5120), "the usual core-protocol maximum");
        assert!(!fits_put_image(PUT_IMAGE_HEADER, 0), "an empty line still needs a byte");
    }

    #[test]
    fn chan_scaling() {
        // 565 visual
        let m = Masks {
            r: 0xf800,
            g: 0x07e0,
            b: 0x001f,
        };
        assert_eq!(m.convert(0xffffff), 0xffff);
        assert_eq!(m.convert(0xff0000), 0xf800);
        assert_eq!(m.convert(0x000000), 0);
        let m = Masks {
            r: 0xff_0000,
            g: 0xff00,
            b: 0xff,
        };
        assert_eq!(m.convert(0x123456), 0x123456);
        // 10-bit channels
        let m = Masks {
            r: 0x3ff0_0000,
            g: 0x000f_fc00,
            b: 0x3ff,
        };
        assert_eq!(m.convert(0xff0000), 0x3fc0_0000);
    }
}
