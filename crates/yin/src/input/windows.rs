//! Input injection on Windows via SendInput API.

use anyhow::Result;
use crossbeam_channel::Receiver;
use tracing::{debug, info};
use yin_yang_proto::{
    keymap::hid_to_windows_scan,
    packets::{InputEvent, InputPacket, MouseButton},
};

pub struct WindowsInputInjector;

impl WindowsInputInjector {
    pub fn start(packet_rx: Receiver<InputPacket>) -> Result<Self> {
        std::thread::Builder::new()
            .name("yin-input-inject".into())
            .spawn(move || inject_loop(packet_rx))?;
        Ok(Self)
    }
}

#[cfg(target_os = "windows")]
fn inject_loop(packet_rx: Receiver<InputPacket>) {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;

    info!("Windows input injection thread started");

    for pkt in packet_rx.iter() {
        let inputs: Vec<INPUT> = match pkt.event {
            InputEvent::MouseMove { dx, dy } => {
                vec![INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx: dx as i32,
                            dy: dy as i32,
                            mouseData: 0,
                            dwFlags: MOUSEEVENTF_MOVE,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                }]
            }
            InputEvent::MouseButton { button, pressed } => {
                let flags = match (button, pressed) {
                    (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
                    (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
                    (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
                    (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
                    (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
                    (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
                    _ => continue,
                };
                vec![INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx: 0,
                            dy: 0,
                            mouseData: 0,
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                }]
            }
            InputEvent::MouseScroll { dx, dy } => {
                let mut inputs = Vec::new();
                let vertical_delta = ((-dy) * 120.0).round() as i32;
                if vertical_delta != 0 {
                    inputs.push(INPUT {
                        r#type: INPUT_MOUSE,
                        Anonymous: INPUT_0 {
                            mi: MOUSEINPUT {
                                dx: 0,
                                dy: 0,
                                mouseData: vertical_delta as u32,
                                dwFlags: MOUSEEVENTF_WHEEL,
                                time: 0,
                                dwExtraInfo: 0,
                            },
                        },
                    });
                }
                let horizontal_delta = (dx * 120.0).round() as i32;
                if horizontal_delta != 0 {
                    inputs.push(INPUT {
                        r#type: INPUT_MOUSE,
                        Anonymous: INPUT_0 {
                            mi: MOUSEINPUT {
                                dx: 0,
                                dy: 0,
                                mouseData: horizontal_delta as u32,
                                dwFlags: MOUSEEVENTF_HWHEEL,
                                time: 0,
                                dwExtraInfo: 0,
                            },
                        },
                    });
                }
                inputs
            }
            InputEvent::KeyEvent {
                hid_usage, pressed, ..
            } => {
                let Some(scan) = hid_to_windows_scan(hid_usage) else {
                    debug!("no Windows scan code for HID 0x{hid_usage:04x}");
                    continue;
                };
                let is_extended = scan > 0xFF;
                let scan_code = scan & 0xFF;
                let mut flags = KEYEVENTF_SCANCODE;
                if is_extended {
                    flags |= KEYEVENTF_EXTENDEDKEY;
                }
                if !pressed {
                    flags |= KEYEVENTF_KEYUP;
                }
                vec![INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: VIRTUAL_KEY(0),
                            wScan: scan_code,
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                }]
            }
        };

        if !inputs.is_empty() {
            unsafe {
                SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
            }
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn inject_loop(packet_rx: Receiver<InputPacket>) {
    info!("Windows input injection stub (non-Windows build)");
    for _ in packet_rx.iter() {}
}
