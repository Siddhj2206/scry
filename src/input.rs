//! Synthetic input injection and focus management.
//!
//! This module implements three strictly separated input layers:
//!
//! - **Pointer** — `wl_seat` pointer motion, buttons and scroll axes,
//!   targetable in global output coordinates or window-local coordinates.
//! - **Physical keys** — evdev/XKB key presses and modifier combos resolved
//!   through the seat's `libxkbcommon` keymap.
//! - **Semantic text** — `zwp_text_input_v3` commit strings when the focused
//!   client provides an enabled text-input instance, with an XKB fallback
//!   that types keymap-mappable characters one keysym at a time.
//!
//! All target/key-name/modifier validation happens up front and returns
//! structured [`Error`]s — invalid input is never silently dropped.

pub(crate) mod keymap;
mod text_input;

use smithay::wayland::selection::{
    data_device::set_data_device_focus, primary_selection::set_primary_focus,
};
use smithay::{
    backend::input::{Axis, AxisSource, ButtonState, KeyState},
    desktop::{Window, WindowSurfaceType},
    input::{
        keyboard::{FilterResult, Keycode, Keysym},
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    reexports::wayland_server::{Resource as _, protocol::wl_surface::WlSurface},
    utils::{Logical, Point, SERIAL_COUNTER, Serial},
    wayland::{seat::WaylandFocus, text_input::TextInputSeat},
};
use tracing::debug;

pub(crate) use self::text_input::TextInputTracker;

use crate::{
    error::{Error, Result},
    event::{CompositorEvent, WindowId},
    state::ScryState,
};

/// The coordinate space a [`CompositorHandle::move_pointer`] target uses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PointerTarget {
    /// Global logical compositor/output coordinates.
    Output,
    /// Coordinates relative to the window's logical top-left corner
    /// (i.e. its [`crate::WindowInfo::geometry`] origin). The pointer only
    /// enters surfaces actually owned by that window — targeting a point
    /// outside the window returns [`Error::OutsideWindow`], and a window
    /// covered by others can still be targeted precisely.
    Window(WindowId),
}

/// A pointer button for [`CompositorHandle::press_pointer_button`] and
/// [`CompositorHandle::release_pointer_button`].
///
/// Variants map to their `linux/input-event-codes.h` codes; [`Self::Other`]
/// accepts additional evdev button codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    /// `BTN_LEFT` (0x110).
    Left,
    /// `BTN_RIGHT` (0x111).
    Right,
    /// `BTN_MIDDLE` (0x112).
    Middle,
    /// `BTN_SIDE` (0x113) — "back" thumb button.
    Side,
    /// `BTN_EXTRA` (0x114) — "forward" thumb button.
    Extra,
    /// `BTN_FORWARD` (0x115).
    Forward,
    /// `BTN_BACK` (0x116).
    Back,
    /// `BTN_TASK` (0x117).
    Task,
    /// Any other evdev button code (0x100–0x2FF).
    Other(u32),
}

impl PointerButton {
    /// The evdev button code to report to clients.
    pub(crate) fn code(self) -> Result<u32> {
        Ok(match self {
            Self::Left => 0x110,
            Self::Right => 0x111,
            Self::Middle => 0x112,
            Self::Side => 0x113,
            Self::Extra => 0x114,
            Self::Forward => 0x115,
            Self::Back => 0x116,
            Self::Task => 0x117,
            Self::Other(code) => {
                if (0x100..=0x2ff).contains(&code) {
                    code
                } else {
                    return Err(Error::InvalidButton(code));
                }
            }
        })
    }
}

// -- command handlers on ScryState -------------------------------------------

impl ScryState {
    /// Milliseconds since compositor start, for wayland event timestamps.
    pub(crate) fn now_ms(&self) -> u32 {
        self.clock.elapsed().as_millis() as u32
    }

    /// `(surface, surface_origin_in_space)` the pointer is logically over.
    fn pointer_focus_at(
        &self,
        point: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.space.element_under(point).and_then(|(window, loc)| {
            window
                .surface_under(point - loc.to_f64(), WindowSurfaceType::ALL)
                .map(|(surface, offset)| (surface, (loc + offset).to_f64()))
        })
    }

    /// Executes [`Command::PointerMotion`](crate::handle::Command::PointerMotion).
    pub(crate) fn cmd_pointer_motion(
        &mut self,
        target: PointerTarget,
        x: f64,
        y: f64,
    ) -> Result<()> {
        if !x.is_finite() || !y.is_finite() {
            return Err(Error::InvalidCoordinates(x, y));
        }
        let (location, focus) = match target {
            PointerTarget::Output => {
                let point: Point<f64, Logical> = (x, y).into();
                (point, self.pointer_focus_at(point))
            }
            PointerTarget::Window(id) => {
                let window = &self
                    .windows
                    .iter()
                    .find(|r| r.id == id)
                    .ok_or(Error::UnknownWindow(id))?
                    .window;
                // `(x, y)` is relative to the window's logical geometry
                // origin — `element_location` is where `map_element` placed
                // it. The surface tree root sits at `render_loc`, shifted by
                // the client-declared window geometry (e.g. CSD shadows).
                let element_loc = self
                    .space
                    .element_location(window)
                    .ok_or(Error::UnknownWindow(id))?;
                let global_point = element_loc.to_f64() + Point::from((x, y));
                let render_loc = element_loc - window.geometry().loc;
                let (surface, offset) = window
                    .surface_under(global_point - render_loc.to_f64(), WindowSurfaceType::ALL)
                    .ok_or(Error::OutsideWindow(id))?;
                (
                    global_point,
                    Some((surface, (render_loc + offset).to_f64())),
                )
            }
        };
        let pointer = self.pointer.clone();
        pointer.motion(
            self,
            focus,
            &MotionEvent {
                location,
                serial: SERIAL_COUNTER.next_serial(),
                time: self.now_ms(),
            },
        );
        // Wayland clients group pointer events by `wl_pointer.frame`:
        // GTK defers dispatch until a frame closes the batch, so a
        // press emitted before the frame that carries `enter` lands
        // against stale pointer state (real GTK4 ignores the first
        // synthetic click). Frame every batch like real compositors do.
        pointer.frame(self);
        Ok(())
    }

    /// Executes [`Command::PointerButton`](crate::handle::Command::PointerButton).
    pub(crate) fn cmd_pointer_button(&mut self, button: u32, pressed: bool) -> Result<()> {
        let pointer = self.pointer.clone();
        pointer.button(
            self,
            &ButtonEvent {
                serial: SERIAL_COUNTER.next_serial(),
                time: self.now_ms(),
                button,
                state: if pressed {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                },
            },
        );
        pointer.frame(self);

        // Click-to-focus: a press raises + focuses the toplevel owning the
        // surface the pointer is on. Popup grabs may reroute/dismiss first —
        // the post-press focus reflects where the event actually landed.
        if pressed
            && let Some(id) = pointer
                .current_focus()
                .and_then(|surface| self.window_id_for_surface(&surface))
            && self.focused != Some(id)
        {
            let _ = self.focus_window_id(id);
        }
        Ok(())
    }

    /// Executes [`Command::PointerScroll`](crate::handle::Command::PointerScroll).
    ///
    /// `x`/`y` are continuous axis deltas; 15.0 units correspond to one wheel
    /// detent and are also reported through `wl_pointer.axis_v120`.
    pub(crate) fn cmd_pointer_scroll(&mut self, x: f64, y: f64) -> Result<()> {
        if !x.is_finite() || !y.is_finite() {
            return Err(Error::InvalidCoordinates(x, y));
        }
        let mut frame = AxisFrame::new(self.now_ms()).source(AxisSource::Wheel);
        if x != 0.0 {
            frame = frame
                .value(Axis::Horizontal, x)
                .v120(Axis::Horizontal, (x * 8.0).round() as i32);
        }
        if y != 0.0 {
            frame = frame
                .value(Axis::Vertical, y)
                .v120(Axis::Vertical, (y * 8.0).round() as i32);
        }
        let pointer = self.pointer.clone();
        pointer.axis(self, frame);
        pointer.frame(self);
        Ok(())
    }

    /// Feeds one physical key state change into XKB and the focused client.
    fn feed_key(&mut self, code: Keycode, state: KeyState) {
        let keyboard = self.keyboard.clone();
        keyboard.input(
            self,
            code,
            state,
            SERIAL_COUNTER.next_serial(),
            self.now_ms(),
            |_, _, _| FilterResult::<()>::Forward,
        );
    }

    /// Presses `code`, tracking the hold so that implicit modifier presses
    /// can be undone only once nothing else needs them.
    fn press_keycode(&mut self, code: Keycode) {
        *self.key_holds.entry(code).or_insert(0) += 1;
        self.feed_key(code, KeyState::Pressed);
    }

    /// Releases `code` if its last hold was released. Returns the modifiers
    /// that were pressed implicitly for this key, for the caller to release.
    fn release_keycode(&mut self, code: Keycode) {
        match self.key_holds.get(&code).copied().unwrap_or(0) {
            0 | 1 => {
                self.key_holds.remove(&code);
                self.feed_key(code, KeyState::Released);
            }
            n => {
                self.key_holds.insert(code, n - 1);
            }
        }
    }

    /// Resolves a keysym through the seat keymap (cached).
    fn resolve_keysym(&mut self, sym: Keysym) -> Option<keymap::ResolvedKey> {
        self.keymap_cache.get(&sym).cloned().flatten().or_else(|| {
            let resolved = self.with_keymap(|keymap| keymap::resolve_keycode(keymap, sym));
            self.keymap_cache.insert(sym, resolved.clone());
            resolved
        })
    }

    /// Resolves a key name through the seat keymap.
    fn resolve_named_key(&mut self, name: &str) -> Result<keymap::ResolvedKey> {
        let sym =
            keymap::key_name_to_keysym(name).ok_or_else(|| Error::UnknownKey(name.to_string()))?;
        self.resolve_keysym(sym)
            .ok_or_else(|| Error::UnknownKey(name.to_string()))
    }

    /// Executes [`Command::KeyEvent`](crate::handle::Command::KeyEvent).
    pub(crate) fn cmd_key_event(&mut self, name: &str, pressed: bool) -> Result<()> {
        self.require_keyboard_focus()?;
        let key = self.resolve_named_key(name)?;
        if pressed {
            for &m in &key.modifiers {
                self.press_keycode(m);
            }
            self.implicit_mods.insert(key.code, key.modifiers.clone());
            self.press_keycode(key.code);
        } else {
            self.release_keycode(key.code);
            let mods = self
                .implicit_mods
                .remove(&key.code)
                .unwrap_or(key.modifiers);
            for m in mods {
                self.release_keycode(m);
            }
        }
        Ok(())
    }

    /// Executes [`Command::KeyCombo`](crate::handle::Command::KeyCombo):
    /// holds `modifiers`, taps `name`, releases the modifiers.
    pub(crate) fn cmd_key_combo(&mut self, name: &str, modifiers: &[String]) -> Result<()> {
        self.require_keyboard_focus()?;
        let key = self.resolve_named_key(name)?;
        let mut codes: Vec<Keycode> = Vec::new();
        for m in modifiers {
            let sym =
                keymap::modifier_keysym(m).ok_or_else(|| Error::UnknownModifier(m.clone()))?;
            let code = self
                .resolve_keysym(sym)
                .map(|r| r.code)
                .ok_or_else(|| Error::UnknownModifier(m.clone()))?;
            if !codes.contains(&code) {
                codes.push(code);
            }
        }
        for &m in &key.modifiers {
            if !codes.contains(&m) {
                codes.push(m);
            }
        }

        for &code in &codes {
            self.press_keycode(code);
        }
        self.press_keycode(key.code);
        self.release_keycode(key.code);
        for &code in codes.iter().rev() {
            self.release_keycode(code);
        }
        Ok(())
    }

    /// Executes [`Command::TypeText`](crate::handle::Command::TypeText).
    ///
    /// Prefers `zwp_text_input_v3` when the focused client has an enabled
    /// instance; otherwise types each character through XKB, adding implicit
    /// modifiers for keysyms living above level 0 of the keymap.
    pub(crate) fn cmd_type_text(&mut self, text: &str) -> Result<()> {
        self.require_keyboard_focus()?;

        // Layer 2: zwp_text_input_v3 for clients that enabled it.
        let ti = self.seat.text_input().clone();
        let mut delivered = false;
        ti.with_focused_text_input(|resource, _surface| {
            if self.text_input_tracker.is_enabled(&resource.id()) {
                let serial = self.text_input_tracker.serial(&resource.id());
                resource.commit_string(Some(text.to_string()));
                resource.done(serial);
                delivered = true;
            }
        });
        if delivered {
            debug!(text, "type_text delivered via zwp_text_input_v3");
            return Ok(());
        }

        // Layer 1 fallback: physical XKB key events. The whole string is
        // resolved up front so an unmappable character fails atomically
        // instead of leaving a partially typed prefix behind.
        let mut keys = Vec::with_capacity(text.len());
        for ch in text.chars() {
            let sym = keymap::char_keysym(ch).ok_or(Error::UnmappableCharacter(ch))?;
            keys.push(
                self.with_keymap(|keymap| keymap::resolve_keycode(keymap, sym))
                    .ok_or(Error::UnmappableCharacter(ch))?,
            );
        }
        for resolved in keys {
            for &m in &resolved.modifiers {
                self.press_keycode(m);
            }
            self.press_keycode(resolved.code);
            self.release_keycode(resolved.code);
            for &m in resolved.modifiers.iter().rev() {
                self.release_keycode(m);
            }
        }
        debug!(text, "type_text delivered via XKB keysym fallback");
        Ok(())
    }

    /// Executes [`Command::FocusWindow`](crate::handle::Command::FocusWindow).
    pub(crate) fn cmd_focus_window(&mut self, id: WindowId) -> Result<()> {
        self.focus_window_id(id)
    }

    // -- focus model ---------------------------------------------------------

    /// Whether a keyboard-focused surface exists right now.
    fn require_keyboard_focus(&self) -> Result<()> {
        if self.keyboard.current_focus().is_some() {
            Ok(())
        } else {
            Err(Error::NoFocusedSurface)
        }
    }

    /// Runs `f` with the seat's compiled XKB keymap.
    fn with_keymap<T>(&mut self, f: impl FnOnce(&smithay::input::keyboard::xkb::Keymap) -> T) -> T {
        let keyboard = self.keyboard.clone();
        let mut f = Some(f);
        keyboard.with_xkb_state(self, |ctx| {
            let xkb = ctx.xkb().lock().unwrap();
            // SAFETY: the keymap reference does not outlive `xkb`.
            let keymap = unsafe { xkb.keymap() };
            f.take().expect("with_xkb_state callback runs once")(keymap)
        })
    }

    /// Raises `id` to the top of the stacking order and points keyboard,
    /// pointer-independent focus and text input at it.
    pub(crate) fn focus_window_id(&mut self, id: WindowId) -> Result<()> {
        let window = self
            .windows
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.window.clone())
            .ok_or(Error::UnknownWindow(id))?;
        self.focus_window(&window);
        Ok(())
    }

    /// Raises `window`, sets the `Activated` pending state on it (cleared on
    /// all others) and moves keyboard + text-input focus to its root surface.
    pub(crate) fn focus_window(&mut self, window: &Window) {
        let serial = SERIAL_COUNTER.next_serial();
        self.space.raise_element(window, true);
        // Deliver the updated activation state to every affected toplevel.
        for record in &self.windows {
            if let Some(toplevel) = record.window.toplevel() {
                toplevel.send_pending_configure();
            }
        }
        let surface = window.wl_surface().map(|s| s.into_owned());
        self.set_keyboard_focus(surface, serial);
    }

    /// Moves keyboard and `zwp_text_input_v3` focus to `surface`
    /// (`None` clears both).
    pub(crate) fn set_keyboard_focus(&mut self, surface: Option<WlSurface>, serial: Serial) {
        let ti = self.seat.text_input().clone();
        ti.leave();
        ti.set_focus(surface.clone());
        let keyboard = self.keyboard.clone();
        keyboard.set_focus(self, surface, serial);
        ti.enter();
    }

    /// Focus fallback: hands focus to the topmost remaining mapped toplevel,
    /// or clears it when none are left.
    pub(crate) fn focus_fallback(&mut self) {
        let topmost = self.space.elements().last().cloned();
        match topmost {
            Some(window) => self.focus_window(&window),
            None => {
                let serial = SERIAL_COUNTER.next_serial();
                self.set_keyboard_focus(None, serial);
                // SeatHandler::focus_changed is only invoked on Some→Some
                // transitions, so clear selection focus and report the
                // cleared focus here.
                set_data_device_focus(&self.display_handle, &self.seat, None);
                set_primary_focus(&self.display_handle, &self.seat, None);
                if self.focused.is_some() {
                    self.focused = None;
                    self.emit(|seq| CompositorEvent::WindowFocused {
                        seq,
                        window_id: None,
                    });
                }
            }
        }
    }
}
