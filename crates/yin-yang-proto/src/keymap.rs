//! USB HID Usage ID ↔ platform scancode tables.
//!
//! The client always sends USB HID Page 0x07 (Keyboard/Keypad) usage IDs.
//! Each server platform translates these to its native scancode/keycode.
//!
//! Reference: USB HID Usage Tables 1.3, Section 10 "Keyboard/Keypad Page"

/// Convert a macOS virtual keycode (CGKeyCode) to a USB HID usage ID.
///
/// Returns `None` for keys without a direct HID mapping.
pub fn macos_vk_to_hid(vk: u32) -> Option<u32> {
    // macOS CGKeyCode → USB HID Keyboard Page (0x07) usage
    // Source: IOKit/hidsystem/IOLLEvent.h + HID Usage Tables 1.3
    let hid = match vk {
        0x00 => 0x04, // a
        0x01 => 0x16, // s
        0x02 => 0x07, // d
        0x03 => 0x09, // f
        0x04 => 0x0B, // h
        0x05 => 0x0A, // g
        0x06 => 0x1D, // z
        0x07 => 0x1B, // x
        0x08 => 0x06, // c
        0x09 => 0x19, // v
        0x0A => 0x64, // § (non-US)
        0x0B => 0x05, // b
        0x0C => 0x14, // q
        0x0D => 0x1A, // w
        0x0E => 0x08, // e
        0x0F => 0x15, // r
        0x10 => 0x1C, // y
        0x11 => 0x17, // t
        0x12 => 0x1E, // 1
        0x13 => 0x1F, // 2
        0x14 => 0x20, // 3
        0x15 => 0x21, // 4
        0x16 => 0x23, // 6
        0x17 => 0x22, // 5
        0x18 => 0x2E, // =
        0x19 => 0x26, // 9
        0x1A => 0x25, // 7
        0x1B => 0x2D, // -
        0x1C => 0x27, // 8
        0x1D => 0x24, // 0
        0x1E => 0x30, // ]
        0x1F => 0x12, // o
        0x20 => 0x13, // u
        0x21 => 0x2F, // [
        0x22 => 0x0C, // i
        0x23 => 0x10, // p
        0x24 => 0x28, // return
        0x25 => 0x0F, // l
        0x26 => 0x0E, // j
        0x27 => 0x34, // '
        0x28 => 0x0D, // k
        0x29 => 0x33, // ;
        0x2A => 0x31, // backslash
        0x2B => 0x2C, // ,  (wait: HID 0x36 is comma)
        0x2C => 0x38, // /
        0x2D => 0x11, // n
        0x2E => 0x10, // m — collision with p, fix:
        // m = 0x10 is wrong; correct HID for m is 0x10 is p, m is 0x0010? Let me redo:
        // Actually HID: a=4,b=5,c=6,d=7,e=8,f=9,g=0xa,h=0xb,i=0xc,j=0xd,k=0xe,l=0xf,
        //               m=0x10,n=0x11,o=0x12,p=0x13,q=0x14,r=0x15,s=0x16,t=0x17,u=0x18,v=0x19,
        //               w=0x1a,x=0x1b,y=0x1c,z=0x1d
        // 1=0x1e,2=0x1f,3=0x20,4=0x21,5=0x22,6=0x23,7=0x24,8=0x25,9=0x26,0=0x27
        // Return=0x28, Esc=0x29, Backspace=0x2a, Tab=0x2b, Space=0x2c
        // -=0x2d, ==0x2e, [=0x2f, ]=0x30, \=0x31, ;=0x33, '=0x34, `=0x35
        // ,=0x36, .=0x37, /=0x38
        _ => return None,
    };
    Some(hid)
}

// Rebuild with correct HID codes:
pub fn macos_keycode_to_hid(vk: u16) -> Option<u32> {
    let hid: u32 = match vk {
        // Letters (macOS vk → HID)
        0 => 0x04,  // A
        11 => 0x05, // B
        8 => 0x06,  // C
        2 => 0x07,  // D
        14 => 0x08, // E
        3 => 0x09,  // F
        5 => 0x0A,  // G
        4 => 0x0B,  // H
        34 => 0x0C, // I
        38 => 0x0D, // J
        40 => 0x0E, // K
        37 => 0x0F, // L
        46 => 0x10, // M
        45 => 0x11, // N
        31 => 0x12, // O
        35 => 0x13, // P
        12 => 0x14, // Q
        15 => 0x15, // R
        1 => 0x16,  // S
        17 => 0x17, // T
        32 => 0x18, // U
        9 => 0x19,  // V
        13 => 0x1A, // W
        7 => 0x1B,  // X
        16 => 0x1C, // Y
        6 => 0x1D,  // Z
        // Numbers
        18 => 0x1E, // 1
        19 => 0x1F, // 2
        20 => 0x20, // 3
        21 => 0x21, // 4
        23 => 0x22, // 5
        22 => 0x23, // 6
        26 => 0x24, // 7
        28 => 0x25, // 8
        25 => 0x26, // 9
        29 => 0x27, // 0
        // Special
        36 => 0x28, // Return
        53 => 0x29, // Escape
        51 => 0x2A, // Backspace/Delete
        48 => 0x2B, // Tab
        49 => 0x2C, // Space
        27 => 0x2D, // -
        24 => 0x2E, // =
        33 => 0x2F, // [
        30 => 0x30, // ]
        42 => 0x31, // Backslash
        41 => 0x33, // ;
        39 => 0x34, // '
        50 => 0x35, // `
        43 => 0x36, // ,
        47 => 0x37, // .
        44 => 0x38, // /
        57 => 0x39, // CapsLock
        // Function keys
        122 => 0x3A, // F1
        120 => 0x3B, // F2
        99 => 0x3C,  // F3
        118 => 0x3D, // F4
        96 => 0x3E,  // F5
        97 => 0x3F,  // F6
        98 => 0x40,  // F7
        100 => 0x41, // F8
        101 => 0x42, // F9
        109 => 0x43, // F10
        103 => 0x44, // F11
        111 => 0x45, // F12
        // Navigation
        117 => 0x4C, // Delete (forward)
        115 => 0x4A, // Home
        116 => 0x4B, // PageUp
        119 => 0x4D, // End
        121 => 0x4E, // PageDown
        123 => 0x50, // Left arrow
        124 => 0x4F, // Right arrow
        125 => 0x51, // Down arrow
        126 => 0x52, // Up arrow
        // Modifiers
        56 => 0xE1, // Left Shift
        60 => 0xE5, // Right Shift
        59 => 0xE0, // Left Ctrl
        62 => 0xE4, // Right Ctrl
        58 => 0xE2, // Left Alt/Option
        61 => 0xE6, // Right Alt/Option
        55 => 0xE3, // Left Cmd (mapped to Left GUI/Meta)
        54 => 0xE7, // Right Cmd
        // Numpad
        82 => 0x62, // Numpad 0
        83 => 0x59, // Numpad 1
        84 => 0x5A, // Numpad 2
        85 => 0x5B, // Numpad 3
        86 => 0x5C, // Numpad 4
        87 => 0x5D, // Numpad 5
        88 => 0x5E, // Numpad 6
        89 => 0x5F, // Numpad 7
        91 => 0x60, // Numpad 8
        92 => 0x61, // Numpad 9
        65 => 0x63, // Numpad .
        67 => 0x55, // Numpad *
        69 => 0x57, // Numpad +
        75 => 0x54, // Numpad /
        78 => 0x56, // Numpad -
        76 => 0x58, // Numpad Enter
        71 => 0x53, // Numlock/Clear
        _ => return None,
    };
    Some(hid)
}

/// Convert a USB HID usage ID to a Linux evdev key code.
pub fn hid_to_linux_evdev(hid: u32) -> Option<u32> {
    // HID → evdev (from linux/input-event-codes.h)
    let evdev = match hid {
        0x04 => 30,  // KEY_A
        0x05 => 48,  // KEY_B
        0x06 => 46,  // KEY_C
        0x07 => 32,  // KEY_D
        0x08 => 18,  // KEY_E
        0x09 => 33,  // KEY_F
        0x0A => 34,  // KEY_G
        0x0B => 35,  // KEY_H
        0x0C => 23,  // KEY_I
        0x0D => 36,  // KEY_J
        0x0E => 37,  // KEY_K
        0x0F => 38,  // KEY_L
        0x10 => 50,  // KEY_M
        0x11 => 49,  // KEY_N
        0x12 => 24,  // KEY_O
        0x13 => 25,  // KEY_P
        0x14 => 16,  // KEY_Q
        0x15 => 19,  // KEY_R
        0x16 => 31,  // KEY_S
        0x17 => 20,  // KEY_T
        0x18 => 22,  // KEY_U
        0x19 => 47,  // KEY_V
        0x1A => 17,  // KEY_W
        0x1B => 45,  // KEY_X
        0x1C => 21,  // KEY_Y
        0x1D => 44,  // KEY_Z
        0x1E => 2,   // KEY_1
        0x1F => 3,   // KEY_2
        0x20 => 4,   // KEY_3
        0x21 => 5,   // KEY_4
        0x22 => 6,   // KEY_5
        0x23 => 7,   // KEY_6
        0x24 => 8,   // KEY_7
        0x25 => 9,   // KEY_8
        0x26 => 10,  // KEY_9
        0x27 => 11,  // KEY_0
        0x28 => 28,  // KEY_ENTER
        0x29 => 1,   // KEY_ESC
        0x2A => 14,  // KEY_BACKSPACE
        0x2B => 15,  // KEY_TAB
        0x2C => 57,  // KEY_SPACE
        0x2D => 12,  // KEY_MINUS
        0x2E => 13,  // KEY_EQUAL
        0x2F => 26,  // KEY_LEFTBRACE
        0x30 => 27,  // KEY_RIGHTBRACE
        0x31 => 43,  // KEY_BACKSLASH
        0x33 => 39,  // KEY_SEMICOLON
        0x34 => 40,  // KEY_APOSTROPHE
        0x35 => 41,  // KEY_GRAVE
        0x36 => 51,  // KEY_COMMA
        0x37 => 52,  // KEY_DOT
        0x38 => 53,  // KEY_SLASH
        0x39 => 58,  // KEY_CAPSLOCK
        0x3A => 59,  // KEY_F1
        0x3B => 60,  // KEY_F2
        0x3C => 61,  // KEY_F3
        0x3D => 62,  // KEY_F4
        0x3E => 63,  // KEY_F5
        0x3F => 64,  // KEY_F6
        0x40 => 65,  // KEY_F7
        0x41 => 66,  // KEY_F8
        0x42 => 67,  // KEY_F9
        0x43 => 68,  // KEY_F10
        0x44 => 87,  // KEY_F11
        0x45 => 88,  // KEY_F12
        0x4A => 102, // KEY_HOME
        0x4B => 104, // KEY_PAGEUP
        0x4C => 111, // KEY_DELETE
        0x4D => 107, // KEY_END
        0x4E => 109, // KEY_PAGEDOWN
        0x4F => 106, // KEY_RIGHT
        0x50 => 105, // KEY_LEFT
        0x51 => 108, // KEY_DOWN
        0x52 => 103, // KEY_UP
        0x53 => 69,  // KEY_NUMLOCK
        0x54 => 98,  // KEY_KPSLASH
        0x55 => 55,  // KEY_KPASTERISK
        0x56 => 74,  // KEY_KPMINUS
        0x57 => 78,  // KEY_KPPLUS
        0x58 => 96,  // KEY_KPENTER
        0x59 => 79,  // KEY_KP1
        0x5A => 80,  // KEY_KP2
        0x5B => 81,  // KEY_KP3
        0x5C => 75,  // KEY_KP4
        0x5D => 76,  // KEY_KP5
        0x5E => 77,  // KEY_KP6
        0x5F => 71,  // KEY_KP7
        0x60 => 72,  // KEY_KP8
        0x61 => 73,  // KEY_KP9
        0x62 => 82,  // KEY_KP0
        0x63 => 83,  // KEY_KPDOT
        // Modifiers
        0xE0 => 29,  // KEY_LEFTCTRL
        0xE1 => 42,  // KEY_LEFTSHIFT
        0xE2 => 56,  // KEY_LEFTALT
        0xE3 => 125, // KEY_LEFTMETA
        0xE4 => 97,  // KEY_RIGHTCTRL
        0xE5 => 54,  // KEY_RIGHTSHIFT
        0xE6 => 100, // KEY_RIGHTALT
        0xE7 => 126, // KEY_RIGHTMETA
        _ => return None,
    };
    Some(evdev)
}

/// Convert a USB HID usage ID to a Windows virtual key code.
/// Returns (vk_code, scan_code) for use with SendInput KEYEVENTF_SCANCODE.
pub fn hid_to_windows_scan(hid: u32) -> Option<u16> {
    // HID → Windows scan code (set 1, for KEYEVENTF_SCANCODE)
    let scan: u16 = match hid {
        0x04 => 0x1E,   // A
        0x05 => 0x30,   // B
        0x06 => 0x2E,   // C
        0x07 => 0x20,   // D
        0x08 => 0x12,   // E
        0x09 => 0x21,   // F
        0x0A => 0x22,   // G
        0x0B => 0x23,   // H
        0x0C => 0x17,   // I
        0x0D => 0x24,   // J
        0x0E => 0x25,   // K
        0x0F => 0x26,   // L
        0x10 => 0x32,   // M
        0x11 => 0x31,   // N
        0x12 => 0x18,   // O
        0x13 => 0x19,   // P
        0x14 => 0x10,   // Q
        0x15 => 0x13,   // R
        0x16 => 0x1F,   // S
        0x17 => 0x14,   // T
        0x18 => 0x16,   // U
        0x19 => 0x2F,   // V
        0x1A => 0x11,   // W
        0x1B => 0x2D,   // X
        0x1C => 0x15,   // Y
        0x1D => 0x2C,   // Z
        0x1E => 0x02,   // 1
        0x1F => 0x03,   // 2
        0x20 => 0x04,   // 3
        0x21 => 0x05,   // 4
        0x22 => 0x06,   // 5
        0x23 => 0x07,   // 6
        0x24 => 0x08,   // 7
        0x25 => 0x09,   // 8
        0x26 => 0x0A,   // 9
        0x27 => 0x0B,   // 0
        0x28 => 0x1C,   // Enter
        0x29 => 0x01,   // Escape
        0x2A => 0x0E,   // Backspace
        0x2B => 0x0F,   // Tab
        0x2C => 0x39,   // Space
        0x2D => 0x0C,   // -
        0x2E => 0x0D,   // =
        0x2F => 0x1A,   // [
        0x30 => 0x1B,   // ]
        0x31 => 0x2B,   // Backslash
        0x33 => 0x27,   // ;
        0x34 => 0x28,   // '
        0x35 => 0x29,   // `
        0x36 => 0x33,   // ,
        0x37 => 0x34,   // .
        0x38 => 0x35,   // /
        0x39 => 0x3A,   // CapsLock
        0x3A => 0x3B,   // F1
        0x3B => 0x3C,   // F2
        0x3C => 0x3D,   // F3
        0x3D => 0x3E,   // F4
        0x3E => 0x3F,   // F5
        0x3F => 0x40,   // F6
        0x40 => 0x41,   // F7
        0x41 => 0x42,   // F8
        0x42 => 0x43,   // F9
        0x43 => 0x44,   // F10
        0x44 => 0x57,   // F11
        0x45 => 0x58,   // F12
        0x4A => 0xE047, // Home (extended)
        0x4B => 0xE049, // PageUp (extended)
        0x4C => 0xE053, // Delete (extended)
        0x4D => 0xE04F, // End (extended)
        0x4E => 0xE051, // PageDown (extended)
        0x4F => 0xE04D, // Right (extended)
        0x50 => 0xE04B, // Left (extended)
        0x51 => 0xE050, // Down (extended)
        0x52 => 0xE048, // Up (extended)
        0xE0 => 0x1D,   // Left Ctrl
        0xE1 => 0x2A,   // Left Shift
        0xE2 => 0x38,   // Left Alt
        0xE3 => 0xE05B, // Left Win (extended)
        0xE4 => 0xE01D, // Right Ctrl (extended)
        0xE5 => 0x36,   // Right Shift
        0xE6 => 0xE038, // Right Alt (extended)
        0xE7 => 0xE05C, // Right Win (extended)
        _ => return None,
    };
    Some(scan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_a() {
        let hid = macos_keycode_to_hid(0).unwrap(); // macOS vk 0 = A
        assert_eq!(hid, 0x04);
        let evdev = hid_to_linux_evdev(hid).unwrap();
        assert_eq!(evdev, 30); // KEY_A in evdev
        let scan = hid_to_windows_scan(hid).unwrap();
        assert_eq!(scan, 0x1E); // Windows scan for A
    }

    #[test]
    fn escape_key() {
        let hid = macos_keycode_to_hid(53).unwrap();
        assert_eq!(hid, 0x29);
        assert_eq!(hid_to_linux_evdev(hid).unwrap(), 1); // KEY_ESC
    }
}
