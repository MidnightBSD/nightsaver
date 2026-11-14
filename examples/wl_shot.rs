//! Test helper: save a screenshot of the first Wayland output as a binary PPM, using
//! wlr-screencopy. Used to check what nightsaver shows in a headless sway.
//!
//! Usage: wl_shot out.ppm

use std::fs::File;
use std::io::Write;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::fs::FileExt;

use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_output::WlOutput, wl_registry, wl_shm, wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

#[derive(Default)]
struct App {
    shm: Option<WlShm>,
    output: Option<WlOutput>,
    manager: Option<ZwlrScreencopyManagerV1>,
    shot: Option<(File, wl_shm::Format, u32, u32, u32)>,
    done: Option<bool>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(
        app: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, .. } = event {
            match interface.as_str() {
                "wl_shm" => app.shm = Some(registry.bind(name, 1, qh, ())),
                "wl_output" if app.output.is_none() => app.output = Some(registry.bind(name, 1, qh, ())),
                "zwlr_screencopy_manager_v1" => app.manager = Some(registry.bind(name, 1, qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for App {
    fn event(
        app: &mut Self,
        frame: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer { format: WEnum::Value(format), width, height, stride } => {
                // SAFETY: SHM_ANON names an unnamed object; the call returns a new fd or -1.
                let fd = unsafe { libc::shm_open(libc::SHM_ANON, libc::O_RDWR | libc::O_CLOEXEC, 0o600) };
                assert!(fd >= 0, "shm_open failed");
                // SAFETY: `fd` was just returned by shm_open and is owned by nothing else.
                let file = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
                let len = u64::from(stride) * u64::from(height);
                file.set_len(len).expect("set_len");
                let shm = app.shm.as_ref().expect("wl_shm");
                let pool = shm.create_pool(file.as_fd(), len as i32, qh, ());
                let buffer = pool.create_buffer(0, width as i32, height as i32, stride as i32, format, qh, ());
                frame.copy(&buffer);
                app.shot = Some((file, format, width, height, stride));
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => app.done = Some(true),
            zwlr_screencopy_frame_v1::Event::Failed => app.done = Some(false),
            _ => {}
        }
    }
}

delegate_noop!(App: ignore WlShm);
delegate_noop!(App: ignore WlOutput);
delegate_noop!(App: ignore WlBuffer);
delegate_noop!(App: WlShmPool);
delegate_noop!(App: ZwlrScreencopyManagerV1);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = std::env::args().nth(1).ok_or("usage: wl_shot out.ppm")?;
    let conn = Connection::connect_to_env()?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let mut app = App::default();
    conn.display().get_registry(&qh, ());
    queue.roundtrip(&mut app)?;
    let (manager, output) = (
        app.manager.clone().ok_or("the compositor has no wlr-screencopy")?,
        app.output.clone().ok_or("no output")?,
    );
    manager.capture_output(0, &output, &qh, ());
    while app.done.is_none() {
        queue.blocking_dispatch(&mut app)?;
    }
    if app.done != Some(true) {
        return Err("screencopy failed".into());
    }
    let (file, format, width, height, stride) = app.shot.take().ok_or("no buffer")?;
    let mut raw = vec![0u8; stride as usize * height as usize];
    file.read_exact_at(&mut raw, 0)?;
    // Little-endian words: XRGB/ARGB are B,G,R,x in memory, XBGR/ABGR are R,G,B,x.
    let bgr = matches!(format, wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888);
    let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
    for row in raw.chunks_exact(stride as usize) {
        for px in row[..width as usize * 4].chunks_exact(4) {
            let rgb = if bgr { [px[2], px[1], px[0]] } else { [px[0], px[1], px[2]] };
            ppm.extend_from_slice(&rgb);
        }
    }
    File::create(&out)?.write_all(&ppm)?;
    println!("{out}: {width}x{height} {format:?}");
    Ok(())
}
