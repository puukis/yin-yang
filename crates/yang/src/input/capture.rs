//! macOS input capture: keyboard + mouse -> InputPacket -> QUIC stream.
//!
//! Uses `rdev` for global keyboard/mouse capture and CoreGraphics cursor APIs
//! for relative mouse mode while the remote session is active.

use anyhow::Result;
use crossbeam_channel::{RecvTimeoutError, Sender};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};
use yin_yang_proto::{
    keymap::macos_keycode_to_hid,
    packets::{InputEvent, InputPacket, KeyModifiers, MouseButton},
};

#[cfg(target_os = "macos")]
use core_graphics::{
    display::CGDisplay,
    geometry::{CGPoint, CGRect},
};

#[cfg(target_os = "macos")]
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
#[cfg(target_os = "macos")]
use core_graphics::event::{
    CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, EventField,
};

/// A handle to the running input capture worker.
pub struct InputCapture {
    captured: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
}

impl InputCapture {
    /// Start capturing global input events and forwarding them to `event_tx`.
    ///
    /// Capture starts idle. Press `Ctrl+Option+M` locally to toggle
    /// relative-mouse capture on/off.
    pub fn start(event_tx: Sender<InputPacket>) -> Result<Self> {
        let captured = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));

        let worker_captured = captured.clone();
        let worker_shutdown = shutdown.clone();
        std::thread::Builder::new()
            .name("yang-input-capture".into())
            .spawn(move || run_capture(event_tx, worker_captured, worker_shutdown))?;

        info!("input capture ready; press Ctrl+Option+M to toggle mouse capture");

        Ok(Self { captured, shutdown })
    }

    /// Leave relative mouse capture mode.
    pub fn release(&self) {
        set_capture_mode(&self.captured, false);
    }
}

impl Drop for InputCapture {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.release();
    }
}

struct CaptureState {
    event_tx: Sender<InputPacket>,
    captured: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    modifiers: KeyModifiers,
    pressed_keys: HashSet<rdev::Key>,
    remote_pressed_keys: HashSet<u32>,
    suppressed_key_releases: HashSet<rdev::Key>,
    pressed_buttons: HashSet<MouseButton>,
    remote_pressed_buttons: HashSet<MouseButton>,
    suppressed_button_releases: HashSet<MouseButton>,
    last_mouse_position: Option<(f64, f64)>,
    pointer_center: Option<(f64, f64)>,
}

impl CaptureState {
    fn new(
        event_tx: Sender<InputPacket>,
        captured: Arc<AtomicBool>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            event_tx,
            captured,
            shutdown,
            modifiers: KeyModifiers::empty(),
            pressed_keys: HashSet::new(),
            remote_pressed_keys: HashSet::new(),
            suppressed_key_releases: HashSet::new(),
            pressed_buttons: HashSet::new(),
            remote_pressed_buttons: HashSet::new(),
            suppressed_button_releases: HashSet::new(),
            last_mouse_position: None,
            pointer_center: capture_center(),
        }
    }

    fn run(mut self) {
        info!("input capture thread started");

        let (raw_tx, raw_rx) = crossbeam_channel::bounded::<rdev::Event>(512);

        // On macOS, rdev::listen calls TISCopyCurrentKeyboardInputSource() from a
        // background thread on every KeyDown event. On macOS 14+ this function is
        // main-thread-only and raises EXC_BREAKPOINT (SIGTRAP) on any other thread.
        // Use a direct CGEventTap instead, bypassing all TIS calls.
        #[cfg(target_os = "macos")]
        spawn_cg_event_tap(raw_tx, self.captured.clone());

        #[cfg(not(target_os = "macos"))]
        let _listen_thread = std::thread::Builder::new()
            .name("yang-rdev-listen".into())
            .spawn(move || {
                if let Err(err) = rdev::listen(move |event| {
                    let _ = raw_tx.try_send(event);
                }) {
                    warn!("rdev::listen error: {err:?}");
                }
            });

        self.recenter_pointer();

        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                self.release_remote_inputs(now_timestamp_us());
                break;
            }

            match raw_rx.recv_timeout(Duration::from_millis(20)) {
                Ok(event) => self.handle_event(event),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        info!("input capture thread stopped");
    }

    fn handle_event(&mut self, raw_event: rdev::Event) {
        let timestamp_us = event_timestamp_us(&raw_event);

        match raw_event.event_type {
            rdev::EventType::MouseMove { x, y } => self.handle_mouse_move(x, y, timestamp_us),
            rdev::EventType::ButtonPress(button) => {
                self.handle_button_event(button, true, timestamp_us)
            }
            rdev::EventType::ButtonRelease(button) => {
                self.handle_button_event(button, false, timestamp_us)
            }
            rdev::EventType::Wheel { delta_x, delta_y } => {
                self.handle_scroll(delta_x, delta_y, timestamp_us)
            }
            rdev::EventType::KeyPress(key) => self.handle_key_event(key, true, timestamp_us),
            rdev::EventType::KeyRelease(key) => self.handle_key_event(key, false, timestamp_us),
        }
    }

    fn handle_mouse_move(&mut self, x: f64, y: f64, timestamp_us: u64) {
        if !self.is_captured() {
            self.last_mouse_position = None;
            return;
        }

        if let Some((center_x, center_y)) = self.pointer_center {
            let dx = clamp_i16((x - center_x).round());
            let dy = clamp_i16((y - center_y).round());
            if dx == 0 && dy == 0 {
                return;
            }

            self.send_event(InputEvent::MouseMove { dx, dy }, timestamp_us);
            self.recenter_pointer();
            return;
        }

        let Some((last_x, last_y)) = self.last_mouse_position else {
            self.last_mouse_position = Some((x, y));
            return;
        };

        self.last_mouse_position = Some((x, y));
        let dx = clamp_i16((x - last_x).round());
        let dy = clamp_i16((y - last_y).round());
        if dx != 0 || dy != 0 {
            self.send_event(InputEvent::MouseMove { dx, dy }, timestamp_us);
        }
    }

    fn handle_button_event(&mut self, button: rdev::Button, pressed: bool, timestamp_us: u64) {
        let button = map_mouse_button(button);
        if pressed {
            self.pressed_buttons.insert(button);
        } else {
            self.pressed_buttons.remove(&button);
            if self.suppressed_button_releases.remove(&button) {
                return;
            }
        }

        if !self.is_captured() {
            return;
        }

        if pressed {
            self.remote_pressed_buttons.insert(button);
        } else {
            self.remote_pressed_buttons.remove(&button);
        }

        self.send_event(InputEvent::MouseButton { button, pressed }, timestamp_us);
    }

    fn handle_scroll(&mut self, delta_x: i64, delta_y: i64, timestamp_us: u64) {
        if !self.is_captured() {
            return;
        }

        self.send_event(
            InputEvent::MouseScroll {
                dx: delta_x as f32,
                dy: -(delta_y as f32),
            },
            timestamp_us,
        );
    }

    fn handle_key_event(&mut self, key: rdev::Key, pressed: bool, timestamp_us: u64) {
        if pressed {
            self.pressed_keys.insert(key);
        } else {
            self.pressed_keys.remove(&key);
        }

        self.update_modifier_state(key, pressed);

        if pressed && self.should_toggle_capture(key) {
            self.toggle_capture(timestamp_us);
            return;
        }

        if !pressed && self.suppressed_key_releases.remove(&key) {
            return;
        }

        if !self.is_captured() {
            return;
        }

        let Some(hid_usage) = rdev_key_to_hid(key) else {
            debug!("no HID mapping for key {key:?}");
            return;
        };

        if pressed {
            self.remote_pressed_keys.insert(hid_usage);
        } else {
            self.remote_pressed_keys.remove(&hid_usage);
        }

        self.send_event(
            InputEvent::KeyEvent {
                hid_usage,
                pressed,
                modifiers: self.modifiers,
            },
            timestamp_us,
        );
    }

    fn should_toggle_capture(&self, key: rdev::Key) -> bool {
        key == rdev::Key::KeyM
            && self.modifiers.contains(KeyModifiers::CTRL)
            && self.modifiers.contains(KeyModifiers::ALT)
    }

    fn toggle_capture(&mut self, timestamp_us: u64) {
        self.suppressed_key_releases
            .extend(self.pressed_keys.iter().copied());
        self.suppressed_button_releases
            .extend(self.pressed_buttons.iter().copied());

        if self.is_captured() {
            self.release_remote_inputs(timestamp_us);
            set_capture_mode(&self.captured, false);
            self.last_mouse_position = None;
            return;
        }

        set_capture_mode(&self.captured, true);
        self.recenter_pointer();
    }

    fn release_remote_inputs(&mut self, timestamp_us: u64) {
        let buttons_to_release: Vec<_> = self.remote_pressed_buttons.drain().collect();
        for button in buttons_to_release {
            self.send_event(
                InputEvent::MouseButton {
                    button,
                    pressed: false,
                },
                timestamp_us,
            );
        }

        let mut keys_to_release = Vec::new();
        let mut modifiers_to_release = Vec::new();
        for hid_usage in self.remote_pressed_keys.drain() {
            if hid_modifier_flag(hid_usage).is_some() {
                modifiers_to_release.push(hid_usage);
            } else {
                keys_to_release.push(hid_usage);
            }
        }

        keys_to_release.sort_unstable();
        modifiers_to_release.sort_unstable();

        for hid_usage in keys_to_release {
            self.send_event(
                InputEvent::KeyEvent {
                    hid_usage,
                    pressed: false,
                    modifiers: self.modifiers,
                },
                timestamp_us,
            );
        }

        for hid_usage in modifiers_to_release {
            if let Some(flag) = hid_modifier_flag(hid_usage) {
                self.modifiers.remove(flag);
            }
            self.send_event(
                InputEvent::KeyEvent {
                    hid_usage,
                    pressed: false,
                    modifiers: self.modifiers,
                },
                timestamp_us,
            );
        }
    }

    fn recenter_pointer(&mut self) {
        if !self.is_captured() {
            self.pointer_center = capture_center();
            self.last_mouse_position = None;
            return;
        }

        self.pointer_center = capture_center();
        self.last_mouse_position = self.pointer_center;

        if let Some((x, y)) = self.pointer_center {
            if let Err(err) = warp_cursor(x, y) {
                warn!("failed to warp cursor to capture center: {err:#}");
            }
        }
    }

    fn update_modifier_state(&mut self, key: rdev::Key, pressed: bool) {
        match key {
            rdev::Key::ShiftLeft | rdev::Key::ShiftRight => {
                self.modifiers.set(KeyModifiers::SHIFT, pressed);
            }
            rdev::Key::ControlLeft | rdev::Key::ControlRight => {
                self.modifiers.set(KeyModifiers::CTRL, pressed);
            }
            rdev::Key::Alt | rdev::Key::AltGr => {
                self.modifiers.set(KeyModifiers::ALT, pressed);
            }
            rdev::Key::MetaLeft | rdev::Key::MetaRight => {
                self.modifiers.set(KeyModifiers::META, pressed);
            }
            rdev::Key::CapsLock if pressed => {
                self.modifiers.toggle(KeyModifiers::CAPS);
            }
            rdev::Key::NumLock if pressed => {
                self.modifiers.toggle(KeyModifiers::NUMLOCK);
            }
            _ => {}
        }
    }

    fn send_event(&self, event: InputEvent, timestamp_us: u64) {
        if let Err(err) = self.event_tx.try_send(InputPacket {
            timestamp_us,
            event,
        }) {
            debug!("dropping captured input packet: {err:?}");
        }
    }

    fn is_captured(&self) -> bool {
        self.captured.load(Ordering::Relaxed)
    }
}

fn run_capture(
    event_tx: Sender<InputPacket>,
    captured: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) {
    CaptureState::new(event_tx, captured, shutdown).run();
}

fn set_capture_mode(captured: &AtomicBool, enabled: bool) {
    let was_enabled = captured.swap(enabled, Ordering::Relaxed);
    if was_enabled == enabled {
        return;
    }

    if let Err(err) = apply_platform_capture_mode(enabled) {
        warn!(
            "failed to {} capture mode: {err:#}",
            if enabled { "enable" } else { "disable" }
        );
    }

    info!(
        "mouse capture {}",
        if enabled { "enabled" } else { "disabled" }
    );
}

fn event_timestamp_us(event: &rdev::Event) -> u64 {
    event
        .time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

fn now_timestamp_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

fn clamp_i16(value: f64) -> i16 {
    value.clamp(i16::MIN as f64, i16::MAX as f64) as i16
}

fn map_mouse_button(button: rdev::Button) -> MouseButton {
    match button {
        rdev::Button::Left => MouseButton::Left,
        rdev::Button::Right => MouseButton::Right,
        rdev::Button::Middle => MouseButton::Middle,
        rdev::Button::Unknown(n) => MouseButton::Side(n),
    }
}

fn hid_modifier_flag(hid_usage: u32) -> Option<KeyModifiers> {
    match hid_usage {
        0x39 => Some(KeyModifiers::CAPS),
        0x53 => Some(KeyModifiers::NUMLOCK),
        0xE0 | 0xE4 => Some(KeyModifiers::CTRL),
        0xE1 | 0xE5 => Some(KeyModifiers::SHIFT),
        0xE2 | 0xE6 => Some(KeyModifiers::ALT),
        0xE3 | 0xE7 => Some(KeyModifiers::META),
        _ => None,
    }
}

fn rdev_key_to_hid(key: rdev::Key) -> Option<u32> {
    macos_keycode_to_hid(rdev_key_to_macos_vk(key)?)
}

/// Map `rdev::Key` to the corresponding macOS virtual keycode.
fn rdev_key_to_macos_vk(key: rdev::Key) -> Option<u16> {
    use rdev::Key;

    let vk = match key {
        Key::Alt => 58,
        Key::AltGr => 61,
        Key::Backspace => 51,
        Key::CapsLock => 57,
        Key::ControlLeft => 59,
        Key::ControlRight => 62,
        Key::Delete => 117,
        Key::DownArrow => 125,
        Key::End => 119,
        Key::Escape => 53,
        Key::F1 => 122,
        Key::F10 => 109,
        Key::F11 => 103,
        Key::F12 => 111,
        Key::F2 => 120,
        Key::F3 => 99,
        Key::F4 => 118,
        Key::F5 => 96,
        Key::F6 => 97,
        Key::F7 => 98,
        Key::F8 => 100,
        Key::F9 => 101,
        Key::Home => 115,
        Key::LeftArrow => 123,
        Key::MetaLeft => 55,
        Key::MetaRight => 54,
        Key::PageDown => 121,
        Key::PageUp => 116,
        Key::Return => 36,
        Key::RightArrow => 124,
        Key::ShiftLeft => 56,
        Key::ShiftRight => 60,
        Key::Space => 49,
        Key::Tab => 48,
        Key::UpArrow => 126,
        Key::PrintScreen | Key::ScrollLock | Key::Pause => return None,
        Key::NumLock => 71,
        Key::BackQuote => 50,
        Key::Num1 => 18,
        Key::Num2 => 19,
        Key::Num3 => 20,
        Key::Num4 => 21,
        Key::Num5 => 23,
        Key::Num6 => 22,
        Key::Num7 => 26,
        Key::Num8 => 28,
        Key::Num9 => 25,
        Key::Num0 => 29,
        Key::Minus => 27,
        Key::Equal => 24,
        Key::KeyQ => 12,
        Key::KeyW => 13,
        Key::KeyE => 14,
        Key::KeyR => 15,
        Key::KeyT => 17,
        Key::KeyY => 16,
        Key::KeyU => 32,
        Key::KeyI => 34,
        Key::KeyO => 31,
        Key::KeyP => 35,
        Key::LeftBracket => 33,
        Key::RightBracket => 30,
        Key::KeyA => 0,
        Key::KeyS => 1,
        Key::KeyD => 2,
        Key::KeyF => 3,
        Key::KeyH => 4,
        Key::KeyG => 5,
        Key::KeyZ => 6,
        Key::KeyX => 7,
        Key::KeyC => 8,
        Key::KeyV => 9,
        Key::IntlBackslash => 10,
        Key::KeyB => 11,
        Key::KeyL => 37,
        Key::KeyJ => 38,
        Key::Quote => 39,
        Key::KeyK => 40,
        Key::SemiColon => 41,
        Key::BackSlash => 42,
        Key::Comma => 43,
        Key::Slash => 44,
        Key::KeyN => 45,
        Key::KeyM => 46,
        Key::Dot => 47,
        Key::Insert => 114,
        Key::KpReturn => 76,
        Key::KpMinus => 78,
        Key::KpPlus => 69,
        Key::KpMultiply => 67,
        Key::KpDivide => 75,
        Key::Kp0 => 82,
        Key::Kp1 => 83,
        Key::Kp2 => 84,
        Key::Kp3 => 85,
        Key::Kp4 => 86,
        Key::Kp5 => 87,
        Key::Kp6 => 88,
        Key::Kp7 => 89,
        Key::Kp8 => 91,
        Key::Kp9 => 92,
        Key::KpDelete => 65,
        Key::Function => 63,
        Key::Unknown(_) => return None,
    };

    Some(vk)
}

/// Spawn a dedicated thread that installs a listen-only CGEventTap and feeds
/// converted `rdev::Event` values into `raw_tx`. Unlike `rdev::listen`, this
/// never calls `TISCopyCurrentKeyboardInputSource`, which is main-thread-only
/// on macOS 14+ and causes EXC_BREAKPOINT when called from a background thread.
///
/// `captured` is shared with `CaptureState` so the callback can detect when the
/// cursor is locked and switch from absolute location to raw delta reporting.
/// When captured, `CGAssociateMouseAndMouseCursorPosition(false)` freezes the
/// on-screen cursor, so `event.location()` always returns the frozen position
/// (delta = 0). Using the raw delta fields bypasses the frozen cursor.
#[cfg(target_os = "macos")]
fn spawn_cg_event_tap(raw_tx: crossbeam_channel::Sender<rdev::Event>, captured: Arc<AtomicBool>) {
    std::thread::Builder::new()
        .name("yang-cg-event-tap".into())
        .spawn(move || {
            let tap = CGEventTap::new(
                CGEventTapLocation::Session,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::ListenOnly,
                vec![
                    CGEventType::KeyDown,
                    CGEventType::KeyUp,
                    CGEventType::FlagsChanged,
                    CGEventType::LeftMouseDown,
                    CGEventType::LeftMouseUp,
                    CGEventType::RightMouseDown,
                    CGEventType::RightMouseUp,
                    CGEventType::OtherMouseDown,
                    CGEventType::OtherMouseUp,
                    CGEventType::MouseMoved,
                    CGEventType::LeftMouseDragged,
                    CGEventType::RightMouseDragged,
                    CGEventType::OtherMouseDragged,
                    CGEventType::ScrollWheel,
                ],
                move |_proxy, ev_type, event| {
                    let is_captured = captured.load(Ordering::Relaxed);
                    let rdev_event_type = match ev_type {
                        CGEventType::KeyDown | CGEventType::KeyUp => {
                            let keycode = event
                                .get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE)
                                as u16;
                            let key = cg_keycode_to_rdev_key(keycode)
                                .unwrap_or(rdev::Key::Unknown(keycode as u32));
                            if matches!(ev_type, CGEventType::KeyDown) {
                                rdev::EventType::KeyPress(key)
                            } else {
                                rdev::EventType::KeyRelease(key)
                            }
                        }
                        CGEventType::FlagsChanged => {
                            let keycode = event
                                .get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE)
                                as u16;
                            let key = cg_keycode_to_rdev_key(keycode)
                                .unwrap_or(rdev::Key::Unknown(keycode as u32));
                            let flags = event.get_flags();
                            if modifier_key_pressed(keycode, flags) {
                                rdev::EventType::KeyPress(key)
                            } else {
                                rdev::EventType::KeyRelease(key)
                            }
                        }
                        CGEventType::LeftMouseDown => {
                            rdev::EventType::ButtonPress(rdev::Button::Left)
                        }
                        CGEventType::LeftMouseUp => {
                            rdev::EventType::ButtonRelease(rdev::Button::Left)
                        }
                        CGEventType::RightMouseDown => {
                            rdev::EventType::ButtonPress(rdev::Button::Right)
                        }
                        CGEventType::RightMouseUp => {
                            rdev::EventType::ButtonRelease(rdev::Button::Right)
                        }
                        CGEventType::OtherMouseDown => {
                            let n = event.get_integer_value_field(
                                EventField::MOUSE_EVENT_BUTTON_NUMBER,
                            ) as u8;
                            rdev::EventType::ButtonPress(rdev::Button::Unknown(n))
                        }
                        CGEventType::OtherMouseUp => {
                            let n = event.get_integer_value_field(
                                EventField::MOUSE_EVENT_BUTTON_NUMBER,
                            ) as u8;
                            rdev::EventType::ButtonRelease(rdev::Button::Unknown(n))
                        }
                        CGEventType::MouseMoved
                        | CGEventType::LeftMouseDragged
                        | CGEventType::RightMouseDragged
                        | CGEventType::OtherMouseDragged => {
                            let loc = event.location();
                            if is_captured {
                                // Cursor is frozen by CGAssociateMouseAndMouseCursorPosition.
                                // event.location() returns the fixed frozen position, so
                                // subtracting the center would always give zero delta.
                                // Use raw mouse delta fields and add them to the frozen
                                // cursor position so handle_mouse_move computes real deltas.
                                let rdx = event.get_integer_value_field(
                                    EventField::MOUSE_EVENT_DELTA_X,
                                ) as f64;
                                let rdy = event.get_integer_value_field(
                                    EventField::MOUSE_EVENT_DELTA_Y,
                                ) as f64;
                                rdev::EventType::MouseMove {
                                    x: loc.x + rdx,
                                    y: loc.y + rdy,
                                }
                            } else {
                                rdev::EventType::MouseMove { x: loc.x, y: loc.y }
                            }
                        }
                        CGEventType::ScrollWheel => {
                            let dx = event.get_integer_value_field(
                                EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2,
                            );
                            let dy = event.get_integer_value_field(
                                EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1,
                            );
                            rdev::EventType::Wheel {
                                delta_x: dx,
                                delta_y: dy,
                            }
                        }
                        _ => return None,
                    };

                    let _ = raw_tx.try_send(rdev::Event {
                        event_type: rdev_event_type,
                        time: std::time::SystemTime::now(),
                        name: None,
                    });
                    None
                },
            );

            match tap {
                Ok(tap) => {
                    let loop_source = tap
                        .mach_port
                        .create_runloop_source(0)
                        .expect("create CGEventTap run loop source");
                    let current_loop = CFRunLoop::get_current();
                    unsafe {
                        current_loop.add_source(&loop_source, kCFRunLoopCommonModes);
                    }
                    tap.enable();
                    CFRunLoop::run_current();
                    warn!("CGEventTap run loop exited unexpectedly");
                }
                Err(()) => {
                    warn!("failed to create CGEventTap — check Accessibility/Input Monitoring permissions");
                }
            }
        })
        .expect("spawn CGEventTap thread");
}

/// Returns `true` if the modifier key with the given CGKeyCode is currently
/// active in `flags`. Used to synthesise press/release from `FlagsChanged`.
#[cfg(target_os = "macos")]
fn modifier_key_pressed(keycode: u16, flags: CGEventFlags) -> bool {
    match keycode {
        56 | 60 => flags.contains(CGEventFlags::CGEventFlagShift),
        59 | 62 => flags.contains(CGEventFlags::CGEventFlagControl),
        58 | 61 => flags.contains(CGEventFlags::CGEventFlagAlternate),
        55 | 54 => flags.contains(CGEventFlags::CGEventFlagCommand),
        57 => flags.contains(CGEventFlags::CGEventFlagAlphaShift),
        63 => flags.contains(CGEventFlags::CGEventFlagSecondaryFn),
        _ => false,
    }
}

/// Inversion of `rdev_key_to_macos_vk`: maps a macOS CGKeyCode to the
/// corresponding `rdev::Key` without consulting Text Input Services.
#[cfg(target_os = "macos")]
fn cg_keycode_to_rdev_key(keycode: u16) -> Option<rdev::Key> {
    use rdev::Key;
    let key = match keycode {
        0 => Key::KeyA,
        1 => Key::KeyS,
        2 => Key::KeyD,
        3 => Key::KeyF,
        4 => Key::KeyH,
        5 => Key::KeyG,
        6 => Key::KeyZ,
        7 => Key::KeyX,
        8 => Key::KeyC,
        9 => Key::KeyV,
        10 => Key::IntlBackslash,
        11 => Key::KeyB,
        12 => Key::KeyQ,
        13 => Key::KeyW,
        14 => Key::KeyE,
        15 => Key::KeyR,
        16 => Key::KeyY,
        17 => Key::KeyT,
        18 => Key::Num1,
        19 => Key::Num2,
        20 => Key::Num3,
        21 => Key::Num4,
        22 => Key::Num6,
        23 => Key::Num5,
        24 => Key::Equal,
        25 => Key::Num9,
        26 => Key::Num7,
        27 => Key::Minus,
        28 => Key::Num8,
        29 => Key::Num0,
        30 => Key::RightBracket,
        31 => Key::KeyO,
        32 => Key::KeyU,
        33 => Key::LeftBracket,
        34 => Key::KeyI,
        35 => Key::KeyP,
        36 => Key::Return,
        37 => Key::KeyL,
        38 => Key::KeyJ,
        39 => Key::Quote,
        40 => Key::KeyK,
        41 => Key::SemiColon,
        42 => Key::BackSlash,
        43 => Key::Comma,
        44 => Key::Slash,
        45 => Key::KeyN,
        46 => Key::KeyM,
        47 => Key::Dot,
        48 => Key::Tab,
        49 => Key::Space,
        50 => Key::BackQuote,
        51 => Key::Backspace,
        53 => Key::Escape,
        54 => Key::MetaRight,
        55 => Key::MetaLeft,
        56 => Key::ShiftLeft,
        57 => Key::CapsLock,
        58 => Key::Alt,
        59 => Key::ControlLeft,
        60 => Key::ShiftRight,
        61 => Key::AltGr,
        62 => Key::ControlRight,
        63 => Key::Function,
        65 => Key::KpDelete,
        67 => Key::KpMultiply,
        69 => Key::KpPlus,
        71 => Key::NumLock,
        75 => Key::KpDivide,
        76 => Key::KpReturn,
        78 => Key::KpMinus,
        82 => Key::Kp0,
        83 => Key::Kp1,
        84 => Key::Kp2,
        85 => Key::Kp3,
        86 => Key::Kp4,
        87 => Key::Kp5,
        88 => Key::Kp6,
        89 => Key::Kp7,
        91 => Key::Kp8,
        92 => Key::Kp9,
        96 => Key::F5,
        97 => Key::F6,
        98 => Key::F7,
        99 => Key::F3,
        100 => Key::F8,
        101 => Key::F9,
        103 => Key::F11,
        109 => Key::F10,
        111 => Key::F12,
        114 => Key::Insert,
        115 => Key::Home,
        116 => Key::PageUp,
        117 => Key::Delete,
        118 => Key::F4,
        119 => Key::End,
        120 => Key::F2,
        121 => Key::PageDown,
        122 => Key::F1,
        123 => Key::LeftArrow,
        124 => Key::RightArrow,
        125 => Key::DownArrow,
        126 => Key::UpArrow,
        _ => return None,
    };
    Some(key)
}

#[cfg(target_os = "macos")]
fn apply_platform_capture_mode(enabled: bool) -> Result<()> {
    let display = CGDisplay::main();
    CGDisplay::associate_mouse_and_mouse_cursor_position(!enabled)
        .map_err(|err| anyhow::anyhow!("CGAssociateMouseAndMouseCursorPosition failed: {err}"))?;
    if enabled {
        display
            .hide_cursor()
            .map_err(|err| anyhow::anyhow!("CGDisplayHideCursor failed: {err}"))?;
    } else {
        display
            .show_cursor()
            .map_err(|err| anyhow::anyhow!("CGDisplayShowCursor failed: {err}"))?;
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn apply_platform_capture_mode(_enabled: bool) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn capture_center() -> Option<(f64, f64)> {
    let bounds: CGRect = CGDisplay::main().bounds();
    Some((
        bounds.origin.x + (bounds.size.width / 2.0),
        bounds.origin.y + (bounds.size.height / 2.0),
    ))
}

#[cfg(not(target_os = "macos"))]
fn capture_center() -> Option<(f64, f64)> {
    None
}

#[cfg(target_os = "macos")]
fn warp_cursor(x: f64, y: f64) -> Result<()> {
    CGDisplay::warp_mouse_cursor_position(CGPoint::new(x, y))
        .map_err(|err| anyhow::anyhow!("CGWarpMouseCursorPosition failed: {err}"))?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn warp_cursor(_x: f64, _y: f64) -> Result<()> {
    Ok(())
}
