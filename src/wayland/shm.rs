//! `wl_shm` buffers for the Wayland backend.
//!
//! Each buffer is its own anonymous shared-memory object. Pixels are put into it with
//! ordinary positioned file writes, so nothing is mapped into this process and no `unsafe`
//! is needed beyond creating the object. A `Framebuffer` pixel (0x00RRGGBB) written
//! little-endian is exactly `wl_shm` XRGB8888.

use std::fs::File;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use wayland_client::protocol::{wl_buffer::WlBuffer, wl_shm, wl_shm::WlShm};
use wayland_client::{Dispatch, QueueHandle};

use crate::render::Framebuffer;

/// Shared with the buffer's event handler: true from attach until the compositor releases.
pub type Busy = Arc<AtomicBool>;

/// One XRGB8888 buffer of a fixed size.
pub struct Buffer {
    pub wl: WlBuffer,
    file: File,
    pub width: u32,
    pub height: u32,
    busy: Busy,
}

/// An anonymous shared-memory object of `len` bytes (zero-filled, which is black).
fn anonymous(len: u64) -> std::io::Result<File> {
    // SAFETY: SHM_ANON is the documented name for an unnamed object; the call has no
    // other preconditions and returns a new descriptor or -1.
    let fd = unsafe { libc::shm_open(libc::SHM_ANON, libc::O_RDWR | libc::O_CLOEXEC, 0o600) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` was just returned by shm_open and is owned by nothing else.
    let file = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    file.set_len(len)?;
    Ok(file)
}

impl Buffer {
    /// A black buffer of `width` x `height` pixels.
    pub fn new<S>(shm: &WlShm, qh: &QueueHandle<S>, width: u32, height: u32) -> std::io::Result<Buffer>
    where
        S: Dispatch<WlBuffer, Busy> + Dispatch<wayland_client::protocol::wl_shm_pool::WlShmPool, ()> + 'static,
    {
        let invalid = || std::io::Error::new(std::io::ErrorKind::InvalidInput, "buffer size out of range");
        let stride = width.checked_mul(4).ok_or_else(invalid)?;
        let len = u64::from(stride) * u64::from(height);
        let (w, h, s, l) = (
            i32::try_from(width).map_err(|_| invalid())?,
            i32::try_from(height).map_err(|_| invalid())?,
            i32::try_from(stride).map_err(|_| invalid())?,
            i32::try_from(len).map_err(|_| invalid())?,
        );
        if w == 0 || h == 0 {
            return Err(invalid());
        }
        let file = anonymous(len)?;
        let busy: Busy = Arc::new(AtomicBool::new(false));
        let pool = shm.create_pool(file.as_fd(), l, qh, ());
        let wl = pool.create_buffer(0, w, h, s, wl_shm::Format::Xrgb8888, qh, busy.clone());
        // The buffer keeps the pool's storage alive.
        pool.destroy();
        Ok(Buffer { wl, file, width, height, busy })
    }

    /// The compositor may still be reading this buffer.
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }

    /// Call when the buffer is attached and committed.
    pub fn mark_busy(&self) {
        self.busy.store(true, Ordering::Release);
    }

    /// Copy `fb` into the buffer. `scratch` is reused between frames to avoid allocating.
    pub fn write(&self, fb: &Framebuffer, scratch: &mut Vec<u8>) -> std::io::Result<()> {
        scratch.clear();
        scratch.reserve(fb.pixels.len() * 4);
        for p in &fb.pixels {
            scratch.extend_from_slice(&p.to_le_bytes());
        }
        self.file.write_all_at(scratch, 0)
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        self.wl.destroy();
    }
}

/// `wl_buffer.release` handling, for the backend's `Dispatch<WlBuffer, Busy>` impl.
pub fn released(busy: &Busy) {
    busy.store(false, Ordering::Release);
}
