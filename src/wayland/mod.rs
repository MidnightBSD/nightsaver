//! The Wayland implementation of `display::Backend`.
//!
//! Blanking: one `wlr-layer-shell` surface per output on the overlay layer, with exclusive
//! keyboard focus. That covers the screen and delivers input, but a layer surface is an
//! ordinary client surface: it is **not** a lock.
//!
//! Locking: `ext-session-lock-v1`. Lock-grade exclusive input (`acquire_exclusive` with
//! `for_lock`) means the compositor has sent `locked`, and nothing less: a lock we only
//! asked for is still "not held", so the daemon keeps the lock pending and shows its NOT
//! LOCKED notice on the layer surfaces. The session lock is ended with
//! `unlock_and_destroy` in exactly one place, `release_exclusive`, which the daemon only
//! reaches after a successful authentication. Dropping the backend never unlocks: if the
//! daemon dies while the session is locked, the compositor keeps it locked.
//!
//! Compositors without `ext-session-lock-v1` (KWin) get blanking only (`can_lock` false);
//! compositors without layer-shell or idle-notify (GNOME) are refused in `open`.
//!
//! There is no display-side control channel here: commands arrive on the control socket
//! (`crate::control`), so `publish_status` and `respond` have nothing to do.

mod shm;

use std::collections::VecDeque;
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant, SystemTime};

use wayland_client::protocol::{
    wl_buffer::{self, WlBuffer},
    wl_compositor::WlCompositor,
    wl_keyboard::{self, WlKeyboard},
    wl_output::{self, WlOutput},
    wl_pointer::{self, WlPointer},
    wl_registry::{self, WlRegistry},
    wl_seat::{self, WlSeat},
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::ExtIdleNotifierV1,
};
use wayland_protocols::ext::session_lock::v1::client::{
    ext_session_lock_manager_v1::ExtSessionLockManagerV1,
    ext_session_lock_surface_v1::{self, ExtSessionLockSurfaceV1},
    ext_session_lock_v1::{self, ExtSessionLockV1},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};
use wayland_protocols_wlr::output_power_management::v1::client::{
    zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1,
    zwlr_output_power_v1::{self, ZwlrOutputPowerV1},
};
use xkbcommon::xkb;

use crate::compat::protocol::Status;
use crate::config::DpmsConfig;
use crate::display::{
    Backend, BackendResult, Capabilities, DisplayEvent, MAX_MONITORS, Monitor, SurfaceId,
};
use crate::input::{KeyInput, classify};
use crate::render::Framebuffer;
use shm::{Buffer, Busy};

/// The idle notification fires this long after the last input; `idle_time` adds it back.
const IDLE_GRANULARITY: Duration = Duration::from_secs(1);
/// How long `blank`/`relayout` wait for the compositor to size new surfaces.
const CONFIGURE_WAIT: Duration = Duration::from_secs(2);
/// Buffers kept per surface: the one on screen, the one being drawn, and a spare.
const MAX_BUFFERS: usize = 3;

/// What a surface is to the compositor.
enum Role {
    Layer(ZwlrLayerSurfaceV1),
    Lock(ExtSessionLockSurfaceV1),
}

/// One of our surfaces on one output.
struct Surf {
    wl: WlSurface,
    role: Role,
    /// Size in surface coordinates from the last configure; `None` until the first one.
    size: Option<(u32, u32)>,
    buffers: Vec<Buffer>,
}

impl Surf {
    fn destroy(mut self) {
        self.buffers.clear();
        match &self.role {
            Role::Layer(l) => l.destroy(),
            Role::Lock(l) => l.destroy(),
        }
        self.wl.destroy();
    }
}

/// One output and what we have put on it. Its registry name doubles as the `SurfaceId`.
struct Slot {
    name: u32,
    output: WlOutput,
    scale: i32,
    layer: Option<Surf>,
    lock: Option<Surf>,
    power: Option<ZwlrOutputPowerV1>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LockPhase {
    /// Asked for; the compositor has not answered.
    Requested,
    /// The compositor sent `locked`: the session is locked and input is ours alone.
    Held,
    /// The compositor sent `finished`: refused, or taken away. The object is defunct.
    Finished,
}

struct Lock {
    obj: ExtSessionLockV1,
    phase: LockPhase,
}

/// Everything the event handlers touch.
struct State {
    qh: QueueHandle<State>,
    events: VecDeque<DisplayEvent>,
    compositor: Option<(WlCompositor, u32)>,
    shm: Option<WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    lock_manager: Option<ExtSessionLockManagerV1>,
    idle_notifier: Option<ExtIdleNotifierV1>,
    power_manager: Option<ZwlrOutputPowerManagerV1>,
    seat: Option<WlSeat>,
    keyboard: Option<WlKeyboard>,
    pointer: Option<WlPointer>,
    xkb_context: xkb::Context,
    xkb_state: Option<xkb::State>,
    idle_notification: Option<ExtIdleNotificationV1>,
    /// When the user went idle (already includes `IDLE_GRANULARITY`); `None` while active.
    idle_since: Option<Instant>,
    outputs: Vec<Slot>,
    /// Between `blank` and `unblank`.
    active: bool,
    lock: Option<Lock>,
    /// Where the pointer last was on the surface it is over (surface coordinates differ
    /// from surface to surface, so only differences are meaningful).
    pointer_local: Option<(f64, f64)>,
    /// The pointer's travel added up into one made-up coordinate system; this is what the
    /// daemon sees as the pointer position.
    pointer_virtual: (f64, f64),
    /// A frame did not fit its surface; `OutputsChanged` was queued and not yet handled.
    resize_reported: bool,
    scratch: Vec<u8>,
}

impl State {
    fn slot(&mut self, name: u32) -> Option<&mut Slot> {
        self.outputs.iter_mut().find(|s| s.name == name)
    }

    fn lock_held(&self) -> bool {
        self.lock.as_ref().is_some_and(|l| l.phase == LockPhase::Held)
    }

    /// The surface of `slot` that is on screen: its lock surface while the session lock is
    /// held, otherwise its layer surface. `None` until that surface has a size.
    fn target(slot: &mut Slot, lock_held: bool) -> Option<(&mut Surf, i32)> {
        let scale = slot.scale.max(1);
        let lock_ready = lock_held && slot.lock.as_ref().is_some_and(|s| s.size.is_some());
        let surf = if lock_ready { slot.lock.as_mut() } else { slot.layer.as_mut() };
        surf.filter(|s| s.size.is_some()).map(|s| (s, scale))
    }

    /// Size in pixels of what `slot` shows, as the daemon must draw it.
    fn pixel_size(slot: &Slot, lock_held: bool) -> Option<(u32, u32)> {
        let scale = slot.scale.max(1) as u32;
        let lock_size = if lock_held { slot.lock.as_ref().and_then(|s| s.size) } else { None };
        let (w, h) = lock_size.or_else(|| slot.layer.as_ref().and_then(|s| s.size))?;
        Some((w.checked_mul(scale)?, h.checked_mul(scale)?))
    }

    fn new_surface(&self) -> Option<WlSurface> {
        self.compositor.as_ref().map(|(c, _)| c.create_surface(&self.qh, ()))
    }

    /// Give every output a layer surface, and a lock surface while a session lock exists.
    fn reconcile(&mut self) {
        let lock = self.lock.as_ref().filter(|l| l.phase != LockPhase::Finished).map(|l| l.obj.clone());
        for i in 0..self.outputs.len() {
            if self.outputs[i].layer.is_none()
                && let (Some(shell), Some(wl)) = (self.layer_shell.clone(), self.new_surface())
            {
                let slot = &mut self.outputs[i];
                let role = shell.get_layer_surface(
                    &wl,
                    Some(&slot.output),
                    Layer::Overlay,
                    "nightsaver".to_owned(),
                    &self.qh,
                    slot.name,
                );
                role.set_size(0, 0);
                role.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
                role.set_exclusive_zone(-1);
                role.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
                wl.commit();
                slot.layer = Some(Surf { wl, role: Role::Layer(role), size: None, buffers: Vec::new() });
            }
            if self.outputs[i].lock.is_none()
                && let (Some(lock), Some(wl)) = (lock.as_ref(), self.new_surface())
            {
                let slot = &mut self.outputs[i];
                let role = lock.get_lock_surface(&wl, &slot.output, &self.qh, slot.name);
                slot.lock = Some(Surf { wl, role: Role::Lock(role), size: None, buffers: Vec::new() });
            }
        }
    }

    /// A surface was (re)sized: show black on it at once, as an X window background would,
    /// so it is mapped and covering before the daemon draws its first frame.
    fn configured(&mut self, name: u32, lock_surface: bool, width: u32, height: u32) {
        let (Some(shm), qh) = (self.shm.clone(), self.qh.clone()) else {
            return;
        };
        let version = self.compositor.as_ref().map_or(1, |(_, v)| *v);
        let active = self.active;
        let Some(slot) = self.slot(name) else {
            return;
        };
        let scale = slot.scale.max(1);
        let surf = if lock_surface { slot.lock.as_mut() } else { slot.layer.as_mut() };
        let Some(surf) = surf else {
            return;
        };
        let resized = surf.size.is_some_and(|old| old != (width, height));
        surf.size = Some((width, height));
        surf.buffers.retain(|b| b.is_busy());
        let (pw, ph) = (width.saturating_mul(scale as u32), height.saturating_mul(scale as u32));
        match Buffer::new(&shm, &qh, pw, ph) {
            Ok(b) => {
                show(surf, &b, scale, version);
                surf.buffers.push(b);
            }
            Err(e) => log::error!("cannot allocate a {pw}x{ph} buffer: {e}"),
        }
        if resized && active {
            self.events.push_back(DisplayEvent::OutputsChanged);
        }
        self.events.push_back(DisplayEvent::Redraw(SurfaceId(name)));
    }

    fn ensure_idle_notification(&mut self) {
        if self.idle_notification.is_none()
            && let (Some(notifier), Some(seat)) = (&self.idle_notifier, &self.seat)
        {
            let ms = IDLE_GRANULARITY.as_millis() as u32;
            self.idle_notification = Some(notifier.get_idle_notification(ms, seat, &self.qh, ()));
        }
    }

    /// Remove the lock surfaces and the lock object. `unlock` ends a held lock (the user
    /// authenticated); without it a held lock is left to the compositor, which keeps the
    /// session locked.
    fn discard_lock(&mut self, unlock: bool) {
        for slot in &mut self.outputs {
            if let Some(s) = slot.lock.take() {
                s.destroy();
            }
        }
        if let Some(lock) = self.lock.take() {
            match lock.phase {
                LockPhase::Held if unlock => lock.obj.unlock_and_destroy(),
                // Destroying a held lock is a protocol error; just forget the object.
                LockPhase::Held => {}
                LockPhase::Requested | LockPhase::Finished => lock.obj.destroy(),
            }
        }
    }
}

/// What a pressed key (an evdev code, as `wl_keyboard` sends it) means to the lock dialog.
fn translate_key(st: &xkb::State, key: u32) -> Option<KeyInput> {
    let code = xkb::Keycode::new(key.checked_add(8)?);
    let sym = st.key_get_one_sym(code).raw();
    if st.mod_name_is_active(xkb::MOD_NAME_CTRL, xkb::STATE_MODS_EFFECTIVE) {
        return Some(classify(sym, "", true));
    }
    let mut text = st.key_get_utf8(code);
    let input = classify(sym, &text, false);
    zeroize::Zeroize::zeroize(&mut text);
    Some(input)
}

/// Attach `buffer` to `surf` and commit it.
fn show(surf: &Surf, buffer: &Buffer, scale: i32, compositor_version: u32) {
    surf.wl.set_buffer_scale(scale);
    surf.wl.attach(Some(&buffer.wl), 0, 0);
    if compositor_version >= 4 {
        surf.wl.damage_buffer(0, 0, buffer.width as i32, buffer.height as i32);
    } else {
        surf.wl.damage(0, 0, i32::MAX, i32::MAX);
    }
    surf.wl.commit();
    buffer.mark_busy();
}

pub struct WaylandBackend {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    /// Shortest configured power timeout; `Some` between `power_install` and `power_restore`.
    power_delay: Option<Duration>,
    power_deadline: Option<Instant>,
    powered_off: bool,
}

fn err(what: &str, e: impl std::fmt::Display) -> Box<dyn std::error::Error> {
    format!("{what}: {e}").into()
}

impl WaylandBackend {
    /// Connect to the compositor named by `WAYLAND_DISPLAY` and check that it offers what
    /// a screen saver needs. `lock_configured` only selects how loudly a compositor
    /// without session locking is reported.
    pub fn open(lock_configured: bool) -> BackendResult<WaylandBackend> {
        let conn = Connection::connect_to_env().map_err(|e| err("cannot connect to the Wayland compositor", e))?;
        let mut queue: EventQueue<State> = conn.new_event_queue();
        let qh = queue.handle();
        let mut state = State {
            qh: qh.clone(),
            events: VecDeque::new(),
            compositor: None,
            shm: None,
            layer_shell: None,
            lock_manager: None,
            idle_notifier: None,
            power_manager: None,
            seat: None,
            keyboard: None,
            pointer: None,
            xkb_context: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            xkb_state: None,
            idle_notification: None,
            idle_since: None,
            outputs: Vec::new(),
            active: false,
            lock: None,
            pointer_local: None,
            pointer_virtual: (0.0, 0.0),
            resize_reported: false,
            scratch: Vec::new(),
        };
        conn.display().get_registry(&qh, ());
        // Once for the globals, once more for what binding them announces (seat
        // capabilities, output scale, the keymap).
        for _ in 0..3 {
            queue.roundtrip(&mut state).map_err(|e| err("Wayland start-up failed", e))?;
        }

        let mut missing = Vec::new();
        if state.compositor.is_none() {
            missing.push("wl_compositor");
        }
        if state.shm.is_none() {
            missing.push("wl_shm");
        }
        if state.seat.is_none() {
            missing.push("wl_seat");
        }
        if state.layer_shell.is_none() {
            missing.push("wlr-layer-shell (zwlr_layer_shell_v1)");
        }
        if state.idle_notifier.is_none() {
            missing.push("ext-idle-notify (ext_idle_notifier_v1)");
        }
        if !missing.is_empty() {
            return Err(format!(
                "this Wayland compositor cannot host nightsaver; it lacks: {}. wlroots \
                 compositors (sway, labwc) and KWin provide them, GNOME does not",
                missing.join(", ")
            )
            .into());
        }
        if state.lock_manager.is_none() {
            let what = "this compositor has no ext-session-lock-v1: the screen can be blanked but not locked";
            if lock_configured {
                log::error!("{what}; `lock` is ignored");
            } else {
                log::warn!("{what}");
            }
        }
        state.ensure_idle_notification();
        state.events.clear();
        Ok(WaylandBackend { conn, queue, state, power_delay: None, power_deadline: None, powered_off: false })
    }

    /// Send what is queued, wait up to `timeout` (`None`: do not wait) for the compositor
    /// or one of the `wake` descriptors, and run the handlers for what arrived.
    fn pump(&mut self, timeout: Option<Duration>, wake: &[RawFd]) -> BackendResult<()> {
        fn read(e: impl std::fmt::Display) -> Box<dyn std::error::Error> {
            err("reading Wayland events", e)
        }
        self.queue.dispatch_pending(&mut self.state).map_err(read)?;
        if !self.state.events.is_empty() {
            return Ok(());
        }
        self.queue.flush().map_err(read)?;
        let Some(guard) = self.queue.prepare_read() else {
            self.queue.dispatch_pending(&mut self.state).map_err(read)?;
            return Ok(());
        };
        let ms = match timeout {
            // Round up so we never spin with a 0 ms timeout while time remains.
            Some(t) => t.as_millis().saturating_add(1).min(i32::MAX as u128) as i32,
            None => 0,
        };
        let mut pfds: Vec<libc::pollfd> = std::iter::once(guard.connection_fd().as_raw_fd())
            .chain(wake.iter().copied())
            .map(|fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 })
            .collect();
        // SAFETY: `pfds` is a valid, initialised slice of pollfd that lives for the whole
        // call and nfds is its length; the first fd is kept open by `guard`, the others by
        // the caller for the duration of this call.
        let rc = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, ms) };
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                return Ok(());
            }
            return Err(err("poll on the Wayland connection failed", e));
        }
        if pfds[0].revents != 0 {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(read(e)),
            }
        } else {
            drop(guard);
        }
        self.queue.dispatch_pending(&mut self.state).map_err(read)?;
        Ok(())
    }

    /// Talk to the compositor until `done` holds or `limit` has passed.
    fn settle(&mut self, limit: Duration, done: impl Fn(&State) -> bool) -> BackendResult<()> {
        let deadline = Instant::now().checked_add(limit);
        loop {
            self.queue
                .roundtrip(&mut self.state)
                .map_err(|e| err("Wayland round trip failed", e))?;
            let left = deadline.map_or(Duration::ZERO, |d| d.saturating_duration_since(Instant::now()));
            if done(&self.state) || left.is_zero() {
                return Ok(());
            }
            let events = std::mem::take(&mut self.state.events);
            let r = self.pump(Some(left.min(Duration::from_millis(50))), &[]);
            // Keep what was queued before in front of anything `pump` added.
            let newer = std::mem::replace(&mut self.state.events, events);
            self.state.events.extend(newer);
            r?;
        }
    }

    fn flush(&self) {
        if let Err(e) = self.conn.flush() {
            log::debug!("Wayland flush failed: {e}");
        }
    }

    fn set_power(&mut self, on: bool) {
        let Some(manager) = self.state.power_manager.clone() else {
            return;
        };
        let mode = if on { zwlr_output_power_v1::Mode::On } else { zwlr_output_power_v1::Mode::Off };
        let qh = self.state.qh.clone();
        for slot in &mut self.state.outputs {
            let name = slot.name;
            slot.power
                .get_or_insert_with(|| manager.get_output_power(&slot.output, &qh, name))
                .set_mode(mode);
        }
        self.powered_off = !on;
        self.flush();
    }
}

impl Backend for WaylandBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            can_lock: self.state.lock_manager.is_some(),
            power_control: self.state.power_manager.is_some(),
            lock_is_separate: true,
        }
    }

    fn wait_event(
        &mut self,
        timeout: Option<Duration>,
        wake: &[RawFd],
    ) -> BackendResult<Option<DisplayEvent>> {
        if let Some(ev) = self.state.events.pop_front() {
            return Ok(Some(ev));
        }
        self.pump(timeout, wake)?;
        Ok(self.state.events.pop_front())
    }

    fn end_of_batch(&mut self) {
        self.flush();
    }

    fn check_connection(&self) -> BackendResult<()> {
        self.conn.roundtrip().map_err(|e| err("Wayland connection", e))?;
        Ok(())
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.power_deadline
    }

    fn tick(&mut self, now: Instant) {
        if self.power_deadline.is_some_and(|t| now >= t) {
            self.power_deadline = None;
            log::debug!("power timeout: switching the outputs off");
            self.set_power(false);
        }
    }

    fn idle_time(&mut self) -> BackendResult<Option<Duration>> {
        let now = Instant::now();
        Ok(Some(self.state.idle_since.map_or(Duration::ZERO, |t| now.saturating_duration_since(t))))
    }

    fn pointer_position(&self) -> Option<(i32, i32)> {
        let (x, y) = self.state.pointer_virtual;
        Some((x as i32, y as i32))
    }

    fn blank(&mut self) -> BackendResult<usize> {
        self.state.active = true;
        self.state.resize_reported = false;
        let n = self.relayout()?;
        if n == 0 {
            self.unblank();
            return Err("the compositor did not show a blanking surface on any output".into());
        }
        Ok(n)
    }

    fn relayout(&mut self) -> BackendResult<usize> {
        self.state.resize_reported = false;
        self.state.reconcile();
        self.settle(CONFIGURE_WAIT, |s| {
            s.outputs.iter().all(|o| o.layer.as_ref().is_some_and(|l| l.size.is_some()))
        })?;
        Ok(self.surfaces().len())
    }

    fn restore_cover(&mut self) {
        self.state.reconcile();
        self.flush();
    }

    fn unblank(&mut self) {
        // A session lock is only ever ended by `release_exclusive`.
        for slot in &mut self.state.outputs {
            if let Some(s) = slot.layer.take() {
                s.destroy();
            }
        }
        self.state.active = false;
        self.state.resize_reported = false;
        self.flush();
    }

    fn surfaces(&self) -> Vec<(SurfaceId, Monitor)> {
        let held = self.state.lock_held();
        self.state
            .outputs
            .iter()
            .filter_map(|slot| {
                let (w, h) = State::pixel_size(slot, held)?;
                let (width, height) = (u16::try_from(w).ok()?, u16::try_from(h).ok()?);
                (width > 0 && height > 0)
                    .then_some((SurfaceId(slot.name), Monitor { x: 0, y: 0, width, height, primary: false }))
            })
            .take(MAX_MONITORS)
            .collect()
    }

    fn attach(&mut self, _surface: SurfaceId, _width: u32, _height: u32) -> BackendResult<()> {
        Ok(())
    }

    fn detach(&mut self, _surface: SurfaceId) {}

    fn detach_all(&mut self) {}

    fn present(&mut self, surface: SurfaceId, fb: &Framebuffer) -> BackendResult<()> {
        let held = self.state.lock_held();
        let version = self.state.compositor.as_ref().map_or(1, |(_, v)| *v);
        let State { outputs, shm, qh, scratch, events, resize_reported, .. } = &mut self.state;
        let (Some(slot), Some(shm)) = (outputs.iter_mut().find(|s| s.name == surface.0), shm.as_ref())
        else {
            return Ok(());
        };
        if State::pixel_size(slot, held) != Some((fb.width, fb.height)) {
            // Drawn for a size the surface no longer has: ask for new frames instead.
            if !std::mem::replace(resize_reported, true) {
                events.push_back(DisplayEvent::OutputsChanged);
            }
            return Ok(());
        }
        let Some((surf, scale)) = State::target(slot, held) else {
            return Ok(());
        };
        surf.buffers.retain(|b| b.is_busy() || (b.width, b.height) == (fb.width, fb.height));
        let free = surf.buffers.iter().position(|b| !b.is_busy());
        let i = match free {
            Some(i) => i,
            None if surf.buffers.len() < MAX_BUFFERS => {
                surf.buffers.push(Buffer::new(shm, qh, fb.width, fb.height)?);
                surf.buffers.len() - 1
            }
            // The compositor still holds every buffer: skip this frame.
            None => return Ok(()),
        };
        surf.buffers[i].write(fb, scratch)?;
        show(surf, &surf.buffers[i], scale, version);
        self.flush();
        Ok(())
    }

    fn acquire_exclusive(&mut self, timeout: Duration, for_lock: bool) -> BackendResult<()> {
        if !self.state.active {
            return Err("not blanked".into());
        }
        if !for_lock {
            // The layer surfaces ask for exclusive keyboard focus; that is all there is.
            return Ok(());
        }
        let Some(manager) = self.state.lock_manager.clone() else {
            return Err("this compositor has no session lock protocol".into());
        };
        if self.state.lock.as_ref().is_some_and(|l| l.phase == LockPhase::Finished) {
            self.state.discard_lock(false);
        }
        if self.state.lock.is_none() {
            let obj = manager.lock(&self.state.qh, ());
            self.state.lock = Some(Lock { obj, phase: LockPhase::Requested });
        }
        self.state.reconcile();
        self.settle(timeout, |s| s.lock.as_ref().is_none_or(|l| l.phase != LockPhase::Requested))?;
        match self.state.lock.as_ref().map(|l| l.phase) {
            Some(LockPhase::Held) => Ok(()),
            Some(LockPhase::Requested) => {
                Err("the compositor has not confirmed the session lock yet".into())
            }
            Some(LockPhase::Finished) | None => {
                self.state.discard_lock(false);
                self.flush();
                Err("the compositor refused the session lock (is another locker running?)".into())
            }
        }
    }

    fn check_exclusive(&mut self) -> BackendResult<bool> {
        self.pump(None, &[])?;
        Ok(match self.state.lock.as_ref().map(|l| l.phase) {
            Some(LockPhase::Held) => true,
            Some(LockPhase::Requested | LockPhase::Finished) => false,
            None => self.state.active,
        })
    }

    fn release_exclusive(&mut self) {
        if self.state.lock.is_none() {
            return;
        }
        // Learn the lock's fate first: a request the compositor has just granted must be
        // ended with unlock, one it refused must not.
        if let Err(e) = self.queue.roundtrip(&mut self.state) {
            log::warn!("Wayland round trip before unlocking failed: {e}");
        }
        self.state.discard_lock(true);
        // Make sure the compositor has seen the unlock before anything else happens.
        if let Err(e) = self.queue.roundtrip(&mut self.state) {
            log::warn!("Wayland round trip after unlocking failed: {e}");
        }
    }

    fn has_keyboard(&self) -> bool {
        self.state.xkb_state.is_some()
    }

    fn prepare_keyboard(&mut self) {
        if self.state.xkb_state.is_none() {
            log::error!(
                "no keymap from the compositor: locking anyway; no password can be typed, so \
                 this lock cannot be undone from this seat"
            );
        }
    }

    fn power_install(&mut self, cfg: &DpmsConfig) {
        if self.state.power_manager.is_none() {
            return;
        }
        // The compositor has no idle power timers of its own to configure: the first
        // configured stage is timed here (`tick`). Zero means "never" for a stage.
        self.power_delay =
            [cfg.standby, cfg.suspend, cfg.off].into_iter().filter(|d| !d.is_zero()).min();
        self.power_deadline = self.power_delay.and_then(|d| Instant::now().checked_add(d));
        if cfg.quick_off {
            self.power_deadline = None;
            self.set_power(false);
        }
    }

    fn power_off(&mut self) {
        self.power_deadline = None;
        self.set_power(false);
    }

    fn power_on(&mut self) {
        if self.powered_off {
            self.set_power(true);
        }
        if let Some(d) = self.power_delay {
            self.power_deadline = Instant::now().checked_add(d);
        }
    }

    fn power_restore(&mut self) {
        self.power_delay = None;
        self.power_deadline = None;
        if self.powered_off {
            self.set_power(true);
        }
    }

    fn publish_status(&mut self, _status: Status, _since: SystemTime, _hacks: &[u32]) -> BackendResult<()> {
        Ok(())
    }

    fn respond(&mut self, _ok: bool, _msg: &str) {}

    fn flush_response(&mut self, _limit: Duration) {}
}

// ---------------------------------------------------------------------------
// Event handlers
// ---------------------------------------------------------------------------

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global { name, interface, version } => match interface.as_str() {
                "wl_compositor" => {
                    let v = version.min(4);
                    state.compositor = Some((registry.bind(name, v, qh, ()), v));
                }
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, version.min(5), qh, ()));
                    state.ensure_idle_notification();
                }
                "wl_output" => {
                    let output = registry.bind(name, version.min(3), qh, name);
                    state.outputs.push(Slot { name, output, scale: 1, layer: None, lock: None, power: None });
                    if state.active {
                        state.events.push_back(DisplayEvent::OutputsChanged);
                    }
                }
                "zwlr_layer_shell_v1" => {
                    state.layer_shell = Some(registry.bind(name, version.min(4), qh, ()));
                }
                "ext_session_lock_manager_v1" => {
                    state.lock_manager = Some(registry.bind(name, 1, qh, ()));
                }
                "ext_idle_notifier_v1" => {
                    state.idle_notifier = Some(registry.bind(name, 1, qh, ()));
                    state.ensure_idle_notification();
                }
                "zwlr_output_power_manager_v1" => {
                    state.power_manager = Some(registry.bind(name, 1, qh, ()));
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(i) = state.outputs.iter().position(|s| s.name == name) {
                    let slot = state.outputs.remove(i);
                    if let Some(s) = slot.layer {
                        s.destroy();
                    }
                    if let Some(s) = slot.lock {
                        s.destroy();
                    }
                    if let Some(p) = slot.power {
                        p.destroy();
                    }
                    if state.active {
                        state.events.push_back(DisplayEvent::OutputsChanged);
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Scale { factor } = event {
            let active = state.active;
            if let Some(slot) = state.slot(*name)
                && slot.scale != factor.max(1)
            {
                slot.scale = factor.max(1);
                if active {
                    state.events.push_back(DisplayEvent::OutputsChanged);
                }
            }
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = event {
            if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
            if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Never log key data (AGENTS.md rule 3).
        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => {
                if format != WEnum::Value(wl_keyboard::KeymapFormat::XkbV1) {
                    log::warn!("the compositor sent a keymap in an unknown format; ignored");
                    return;
                }
                // SAFETY: `fd` is the keymap descriptor the compositor just sent, of `size`
                // bytes; xkbcommon maps it privately and read-only and copies what it needs
                // before the mapping is dropped.
                let keymap = unsafe {
                    xkb::Keymap::new_from_fd(
                        &state.xkb_context,
                        fd,
                        size as usize,
                        xkb::KEYMAP_FORMAT_TEXT_V1,
                        xkb::KEYMAP_COMPILE_NO_FLAGS,
                    )
                };
                match keymap {
                    Ok(Some(k)) => state.xkb_state = Some(xkb::State::new(&k)),
                    Ok(None) => log::error!("the compositor's keymap does not compile; keys cannot be read"),
                    Err(e) => log::error!("cannot read the compositor's keymap: {e}"),
                }
            }
            wl_keyboard::Event::Modifiers { mods_depressed, mods_latched, mods_locked, group, .. } => {
                if let Some(st) = state.xkb_state.as_mut() {
                    st.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
            }
            wl_keyboard::Event::Key { key, state: WEnum::Value(wl_keyboard::KeyState::Pressed), .. }
                if state.active =>
            {
                let input = state.xkb_state.as_ref().and_then(|st| translate_key(st, key));
                state.events.push_back(DisplayEvent::Key(input));
            }
            _ => {}
        }
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        pointer: &WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // A surface appearing under the pointer is not user activity: hide the cursor
            // and take the position as the starting point for the motion that follows.
            wl_pointer::Event::Enter { serial, surface_x, surface_y, .. } => {
                pointer.set_cursor(serial, None, 0, 0);
                state.pointer_local = Some((surface_x, surface_y));
            }
            wl_pointer::Event::Leave { .. } => state.pointer_local = None,
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                if let Some((lx, ly)) = state.pointer_local.replace((surface_x, surface_y)) {
                    state.pointer_virtual.0 += surface_x - lx;
                    state.pointer_virtual.1 += surface_y - ly;
                    if state.active {
                        let (x, y) = state.pointer_virtual;
                        state.events.push_back(DisplayEvent::Motion { x: x as i32, y: y as i32 });
                    }
                }
            }
            wl_pointer::Event::Button { state: WEnum::Value(wl_pointer::ButtonState::Pressed), .. }
                if state.active =>
            {
                state.events.push_back(DisplayEvent::Button);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, u32> for State {
    fn event(
        state: &mut Self,
        surface: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                surface.ack_configure(serial);
                state.configured(*name, false, width, height);
            }
            zwlr_layer_surface_v1::Event::Closed => {
                // Only forget the surface this event is about (it may already be replaced).
                let active = state.active;
                if let Some(slot) = state.slot(*name)
                    && slot.layer.as_ref().is_some_and(|s| matches!(&s.role, Role::Layer(l) if l == surface))
                {
                    if let Some(s) = slot.layer.take() {
                        s.destroy();
                    }
                    if active {
                        log::warn!("the compositor closed a blanking surface");
                        state.events.push_back(DisplayEvent::SurfacesDisturbed);
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtSessionLockV1, ()> for State {
    fn event(
        state: &mut Self,
        lock: &ExtSessionLockV1,
        event: ext_session_lock_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Events of a lock object we have already given up on do not count.
        let Some(current) = state.lock.as_mut().filter(|l| &l.obj == lock) else {
            return;
        };
        match event {
            ext_session_lock_v1::Event::Locked => current.phase = LockPhase::Held,
            ext_session_lock_v1::Event::Finished => {
                let was_held = current.phase == LockPhase::Held;
                current.phase = LockPhase::Finished;
                if was_held {
                    state.events.push_back(DisplayEvent::ExclusiveLost("the compositor ended the session lock"));
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtSessionLockSurfaceV1, u32> for State {
    fn event(
        state: &mut Self,
        surface: &ExtSessionLockSurfaceV1,
        event: ext_session_lock_surface_v1::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_session_lock_surface_v1::Event::Configure { serial, width, height } = event {
            surface.ack_configure(serial);
            state.configured(*name, true, width, height);
        }
    }
}

impl Dispatch<ExtIdleNotificationV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_idle_notification_v1::Event::Idled => {
                let now = Instant::now();
                state.idle_since = Some(now.checked_sub(IDLE_GRANULARITY).unwrap_or(now));
            }
            ext_idle_notification_v1::Event::Resumed => state.idle_since = None,
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputPowerV1, u32> for State {
    fn event(
        state: &mut Self,
        power: &ZwlrOutputPowerV1,
        event: zwlr_output_power_v1::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_output_power_v1::Event::Failed = event {
            log::warn!("the compositor cannot control the power of an output");
            if let Some(slot) = state.slot(*name)
                && slot.power.as_ref() == Some(power)
            {
                slot.power = None;
            }
            power.destroy();
        }
    }
}

impl Dispatch<WlBuffer, Busy> for State {
    fn event(
        _: &mut Self,
        _: &WlBuffer,
        event: wl_buffer::Event,
        busy: &Busy,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            shm::released(busy);
        }
    }
}

delegate_noop!(State: WlCompositor);
delegate_noop!(State: WlShmPool);
delegate_noop!(State: ZwlrLayerShellV1);
delegate_noop!(State: ExtSessionLockManagerV1);
delegate_noop!(State: ExtIdleNotifierV1);
delegate_noop!(State: ZwlrOutputPowerManagerV1);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlSurface);

#[cfg(test)]
mod tests {
    use super::*;

    /// Key translation against a real xkb keymap (the system's "us" layout): evdev codes
    /// in, lock-dialog input out, with the modifier state the compositor would report.
    #[test]
    fn keys_translate_through_xkb() {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let Some(keymap) =
            xkb::Keymap::new_from_names(&ctx, "evdev", "pc105", "us", "", None, xkb::KEYMAP_COMPILE_NO_FLAGS)
        else {
            eprintln!("skipping keys_translate_through_xkb: no xkb data for the us layout");
            return;
        };
        let mut st = xkb::State::new(&keymap);
        // evdev key codes, from <linux/input-event-codes.h>.
        const KEY_ESC: u32 = 1;
        const KEY_BACKSPACE: u32 = 14;
        const KEY_U: u32 = 22;
        const KEY_ENTER: u32 = 28;
        const KEY_A: u32 = 30;
        const KEY_LEFTSHIFT: u32 = 42;
        assert_eq!(translate_key(&st, KEY_A), Some(KeyInput::Text("a".into())));
        assert_eq!(translate_key(&st, KEY_ENTER), Some(KeyInput::Enter));
        assert_eq!(translate_key(&st, KEY_ESC), Some(KeyInput::Escape));
        assert_eq!(translate_key(&st, KEY_BACKSPACE), Some(KeyInput::Backspace));
        assert_eq!(translate_key(&st, KEY_LEFTSHIFT), Some(KeyInput::Other));

        let shift = 1 << keymap.mod_get_index(xkb::MOD_NAME_SHIFT);
        st.update_mask(shift, 0, 0, 0, 0, 0);
        assert_eq!(translate_key(&st, KEY_A), Some(KeyInput::Text("A".into())));

        let ctrl = 1 << keymap.mod_get_index(xkb::MOD_NAME_CTRL);
        st.update_mask(ctrl, 0, 0, 0, 0, 0);
        assert_eq!(translate_key(&st, KEY_U), Some(KeyInput::Kill), "Ctrl-U clears the buffer");
        assert_eq!(translate_key(&st, KEY_A), Some(KeyInput::Other), "Ctrl-letter is never text");
        assert_eq!(translate_key(&st, u32::MAX), None);
    }
}
