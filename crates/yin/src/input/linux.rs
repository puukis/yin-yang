//! Input injection on Linux via `/dev/uinput`.

use anyhow::{Context, Result};
use crossbeam_channel::Receiver;
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use tracing::{debug, info, warn};
use yin_yang_proto::{
    keymap::hid_to_linux_evdev,
    packets::{InputEvent, InputPacket, MouseButton},
};

/// Injector handle — receives `InputPacket`s and injects them through uinput.
pub struct LinuxInputInjector;

impl LinuxInputInjector {
    /// Start the Linux input injection thread.
    pub fn start(packet_rx: Receiver<InputPacket>) -> Result<Self> {
        std::thread::Builder::new()
            .name("yin-input-inject".into())
            .spawn(move || {
                if let Err(err) = run_with_uinput(packet_rx) {
                    warn!("Linux input injection stopped: {err:#}");
                }
            })?;
        Ok(Self)
    }
}

/// Evdev event structure (matches linux/input.h `struct input_event`).
#[repr(C)]
struct InputEv {
    sec: i64,
    usec: i64,
    r#type: u16,
    code: u16,
    value: i32,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

const UINPUT_IOCTL_BASE: u8 = b'U';
const UI_SET_EVBIT: u64 = nix::request_code_write!(UINPUT_IOCTL_BASE, 100, 4);
const UI_SET_KEYBIT: u64 = nix::request_code_write!(UINPUT_IOCTL_BASE, 101, 4);
const UI_SET_RELBIT: u64 = nix::request_code_write!(UINPUT_IOCTL_BASE, 102, 4);
const UI_DEV_CREATE: u64 = nix::request_code_none!(UINPUT_IOCTL_BASE, 1);
const UI_DEV_DESTROY: u64 = nix::request_code_none!(UINPUT_IOCTL_BASE, 2);
const UI_DEV_SETUP: u64 = nix::request_code_write!(
    UINPUT_IOCTL_BASE,
    3,
    std::mem::size_of::<UinputSetup>() as u64
);

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const SYN_REPORT: u16 = 0;

const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;
const BTN_FORWARD: u16 = 0x115;
const BTN_BACK: u16 = 0x116;
const BTN_TASK: u16 = 0x117;

const EV_KEY_BIT: i32 = 1;
const EV_REL_BIT: i32 = 2;

fn run_with_uinput(packet_rx: Receiver<InputPacket>) -> Result<()> {
    let fd = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/uinput")
        .context("open /dev/uinput — ensure the user has uinput access")?;
    let raw_fd = fd.as_raw_fd();

    configure_device(raw_fd)?;

    info!("uinput virtual device created");

    for packet in packet_rx.iter() {
        match packet.event {
            InputEvent::MouseMove { dx, dy } => {
                write_event(raw_fd, EV_REL, REL_X, dx as i32)?;
                write_event(raw_fd, EV_REL, REL_Y, dy as i32)?;
                syn(raw_fd)?;
            }
            InputEvent::MouseButton { button, pressed } => {
                write_event(
                    raw_fd,
                    EV_KEY,
                    linux_mouse_button_code(button),
                    if pressed { 1 } else { 0 },
                )?;
                syn(raw_fd)?;
            }
            InputEvent::MouseScroll { dx, dy } => {
                if dy.abs() > 0.01 {
                    write_event(raw_fd, EV_REL, REL_WHEEL, -dy.round() as i32)?;
                }
                if dx.abs() > 0.01 {
                    write_event(raw_fd, EV_REL, REL_HWHEEL, dx.round() as i32)?;
                }
                syn(raw_fd)?;
            }
            InputEvent::KeyEvent {
                hid_usage, pressed, ..
            } => {
                let Some(evdev_code) = hid_to_linux_evdev(hid_usage) else {
                    debug!("no evdev mapping for HID 0x{hid_usage:04x}");
                    continue;
                };

                write_event(
                    raw_fd,
                    EV_KEY,
                    evdev_code as u16,
                    if pressed { 1 } else { 0 },
                )?;
                syn(raw_fd)?;
            }
        }
    }

    ioctl_none(raw_fd, UI_DEV_DESTROY).context("destroy uinput device")?;
    info!("uinput virtual device destroyed");
    Ok(())
}

fn configure_device(raw_fd: RawFd) -> Result<()> {
    ioctl_with_i32(raw_fd, UI_SET_EVBIT, EV_KEY_BIT).context("enable EV_KEY")?;
    ioctl_with_i32(raw_fd, UI_SET_EVBIT, EV_REL_BIT).context("enable EV_REL")?;

    for key_code in 0..256i32 {
        ioctl_with_i32(raw_fd, UI_SET_KEYBIT, key_code)
            .with_context(|| format!("enable key bit {key_code}"))?;
    }

    for button_code in [
        BTN_LEFT,
        BTN_RIGHT,
        BTN_MIDDLE,
        BTN_SIDE,
        BTN_EXTRA,
        BTN_FORWARD,
        BTN_BACK,
        BTN_TASK,
    ] {
        ioctl_with_i32(raw_fd, UI_SET_KEYBIT, button_code as i32)
            .with_context(|| format!("enable mouse button bit {button_code:#x}"))?;
    }

    for rel_code in [REL_X, REL_Y, REL_WHEEL, REL_HWHEEL] {
        ioctl_with_i32(raw_fd, UI_SET_RELBIT, rel_code as i32)
            .with_context(|| format!("enable relative axis {rel_code:#x}"))?;
    }

    let mut setup = UinputSetup {
        id: InputId {
            bustype: 0x03,
            vendor: 0x0001,
            product: 0x0001,
            version: 1,
        },
        name: [0u8; 80],
        ff_effects_max: 0,
    };
    let name = b"yin virtual input";
    setup.name[..name.len()].copy_from_slice(name);

    ioctl_with_ptr(raw_fd, UI_DEV_SETUP, &setup as *const UinputSetup)
        .context("configure uinput device")?;
    ioctl_none(raw_fd, UI_DEV_CREATE).context("create uinput device")?;
    Ok(())
}

fn linux_mouse_button_code(button: MouseButton) -> u16 {
    match button {
        MouseButton::Left => BTN_LEFT,
        MouseButton::Right => BTN_RIGHT,
        MouseButton::Middle => BTN_MIDDLE,
        MouseButton::Side(0) => BTN_SIDE,
        MouseButton::Side(1) => BTN_EXTRA,
        MouseButton::Side(2) => BTN_FORWARD,
        MouseButton::Side(3) => BTN_BACK,
        MouseButton::Side(_) => BTN_TASK,
    }
}

fn syn(raw_fd: RawFd) -> Result<()> {
    write_event(raw_fd, EV_SYN, SYN_REPORT, 0)
}

fn write_event(raw_fd: RawFd, r#type: u16, code: u16, value: i32) -> Result<()> {
    let event = InputEv {
        sec: 0,
        usec: 0,
        r#type,
        code,
        value,
    };

    let written = unsafe {
        nix::libc::write(
            raw_fd,
            &event as *const InputEv as *const nix::libc::c_void,
            std::mem::size_of::<InputEv>(),
        )
    };

    if written == std::mem::size_of::<InputEv>() as isize {
        Ok(())
    } else if written < 0 {
        Err(io::Error::last_os_error()).context("write input event")
    } else {
        Err(anyhow::anyhow!(
            "short write to uinput device: wrote {written} bytes"
        ))
    }
}

fn ioctl_with_i32(raw_fd: RawFd, request: u64, value: i32) -> Result<()> {
    let rc = unsafe { nix::libc::ioctl(raw_fd, request, value) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error()).context("uinput ioctl")
    }
}

fn ioctl_with_ptr<T>(raw_fd: RawFd, request: u64, value: *const T) -> Result<()> {
    let rc = unsafe { nix::libc::ioctl(raw_fd, request, value) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error()).context("uinput ioctl")
    }
}

fn ioctl_none(raw_fd: RawFd, request: u64) -> Result<()> {
    let rc = unsafe { nix::libc::ioctl(raw_fd, request) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error()).context("uinput ioctl")
    }
}
