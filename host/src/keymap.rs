// Made by keymap/gen.mjs from Chromium's ui/events/keycodes/dom/dom_code_data.inc: do not edit, run it again.

/// A key: its USB HID usage and its codes on the systems. 0 ( mac: 0xFFFF ) - the system has no such key.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // each system reads its own code
pub struct Key {
    pub usb:   u32,
    /// Windows scan code: 0xE0xx - an extended key.
    pub win:   u16,
    /// Linux evdev code: the X11 keycode is 8 more.
    pub evdev: u16,
    /// macOS virtual key code.
    pub mac:   u16,
}

/// The key of a USB HID usage.
pub fn key(usb: u32) -> Option<&'static Key> { KEYS.binary_search_by_key(&usb, |k| k.usb).ok().map(|i| &KEYS[i]) }

/// Every key, by USB HID usage ascending.
pub const KEYS: &[Key] = &[
    Key { usb: 0x010082, win: 0xe05f, evdev: 0x008e, mac: 0xffff }, // Sleep
    Key { usb: 0x010083, win: 0xe063, evdev: 0x008f, mac: 0xffff }, // WakeUp
    Key { usb: 0x010097, win: 0x0000, evdev: 0x01d0, mac: 0xffff }, // Fn
    Key { usb: 0x070004, win: 0x001e, evdev: 0x001e, mac: 0x0000 }, // KeyA
    Key { usb: 0x070005, win: 0x0030, evdev: 0x0030, mac: 0x000b }, // KeyB
    Key { usb: 0x070006, win: 0x002e, evdev: 0x002e, mac: 0x0008 }, // KeyC
    Key { usb: 0x070007, win: 0x0020, evdev: 0x0020, mac: 0x0002 }, // KeyD
    Key { usb: 0x070008, win: 0x0012, evdev: 0x0012, mac: 0x000e }, // KeyE
    Key { usb: 0x070009, win: 0x0021, evdev: 0x0021, mac: 0x0003 }, // KeyF
    Key { usb: 0x07000a, win: 0x0022, evdev: 0x0022, mac: 0x0005 }, // KeyG
    Key { usb: 0x07000b, win: 0x0023, evdev: 0x0023, mac: 0x0004 }, // KeyH
    Key { usb: 0x07000c, win: 0x0017, evdev: 0x0017, mac: 0x0022 }, // KeyI
    Key { usb: 0x07000d, win: 0x0024, evdev: 0x0024, mac: 0x0026 }, // KeyJ
    Key { usb: 0x07000e, win: 0x0025, evdev: 0x0025, mac: 0x0028 }, // KeyK
    Key { usb: 0x07000f, win: 0x0026, evdev: 0x0026, mac: 0x0025 }, // KeyL
    Key { usb: 0x070010, win: 0x0032, evdev: 0x0032, mac: 0x002e }, // KeyM
    Key { usb: 0x070011, win: 0x0031, evdev: 0x0031, mac: 0x002d }, // KeyN
    Key { usb: 0x070012, win: 0x0018, evdev: 0x0018, mac: 0x001f }, // KeyO
    Key { usb: 0x070013, win: 0x0019, evdev: 0x0019, mac: 0x0023 }, // KeyP
    Key { usb: 0x070014, win: 0x0010, evdev: 0x0010, mac: 0x000c }, // KeyQ
    Key { usb: 0x070015, win: 0x0013, evdev: 0x0013, mac: 0x000f }, // KeyR
    Key { usb: 0x070016, win: 0x001f, evdev: 0x001f, mac: 0x0001 }, // KeyS
    Key { usb: 0x070017, win: 0x0014, evdev: 0x0014, mac: 0x0011 }, // KeyT
    Key { usb: 0x070018, win: 0x0016, evdev: 0x0016, mac: 0x0020 }, // KeyU
    Key { usb: 0x070019, win: 0x002f, evdev: 0x002f, mac: 0x0009 }, // KeyV
    Key { usb: 0x07001a, win: 0x0011, evdev: 0x0011, mac: 0x000d }, // KeyW
    Key { usb: 0x07001b, win: 0x002d, evdev: 0x002d, mac: 0x0007 }, // KeyX
    Key { usb: 0x07001c, win: 0x0015, evdev: 0x0015, mac: 0x0010 }, // KeyY
    Key { usb: 0x07001d, win: 0x002c, evdev: 0x002c, mac: 0x0006 }, // KeyZ
    Key { usb: 0x07001e, win: 0x0002, evdev: 0x0002, mac: 0x0012 }, // Digit1
    Key { usb: 0x07001f, win: 0x0003, evdev: 0x0003, mac: 0x0013 }, // Digit2
    Key { usb: 0x070020, win: 0x0004, evdev: 0x0004, mac: 0x0014 }, // Digit3
    Key { usb: 0x070021, win: 0x0005, evdev: 0x0005, mac: 0x0015 }, // Digit4
    Key { usb: 0x070022, win: 0x0006, evdev: 0x0006, mac: 0x0017 }, // Digit5
    Key { usb: 0x070023, win: 0x0007, evdev: 0x0007, mac: 0x0016 }, // Digit6
    Key { usb: 0x070024, win: 0x0008, evdev: 0x0008, mac: 0x001a }, // Digit7
    Key { usb: 0x070025, win: 0x0009, evdev: 0x0009, mac: 0x001c }, // Digit8
    Key { usb: 0x070026, win: 0x000a, evdev: 0x000a, mac: 0x0019 }, // Digit9
    Key { usb: 0x070027, win: 0x000b, evdev: 0x000b, mac: 0x001d }, // Digit0
    Key { usb: 0x070028, win: 0x001c, evdev: 0x001c, mac: 0x0024 }, // Enter
    Key { usb: 0x070029, win: 0x0001, evdev: 0x0001, mac: 0x0035 }, // Escape
    Key { usb: 0x07002a, win: 0x000e, evdev: 0x000e, mac: 0x0033 }, // Backspace
    Key { usb: 0x07002b, win: 0x000f, evdev: 0x000f, mac: 0x0030 }, // Tab
    Key { usb: 0x07002c, win: 0x0039, evdev: 0x0039, mac: 0x0031 }, // Space
    Key { usb: 0x07002d, win: 0x000c, evdev: 0x000c, mac: 0x001b }, // Minus
    Key { usb: 0x07002e, win: 0x000d, evdev: 0x000d, mac: 0x0018 }, // Equal
    Key { usb: 0x07002f, win: 0x001a, evdev: 0x001a, mac: 0x0021 }, // BracketLeft
    Key { usb: 0x070030, win: 0x001b, evdev: 0x001b, mac: 0x001e }, // BracketRight
    Key { usb: 0x070031, win: 0x002b, evdev: 0x002b, mac: 0x002a }, // Backslash
    Key { usb: 0x070033, win: 0x0027, evdev: 0x0027, mac: 0x0029 }, // Semicolon
    Key { usb: 0x070034, win: 0x0028, evdev: 0x0028, mac: 0x0027 }, // Quote
    Key { usb: 0x070035, win: 0x0029, evdev: 0x0029, mac: 0x0032 }, // Backquote
    Key { usb: 0x070036, win: 0x0033, evdev: 0x0033, mac: 0x002b }, // Comma
    Key { usb: 0x070037, win: 0x0034, evdev: 0x0034, mac: 0x002f }, // Period
    Key { usb: 0x070038, win: 0x0035, evdev: 0x0035, mac: 0x002c }, // Slash
    Key { usb: 0x070039, win: 0x003a, evdev: 0x003a, mac: 0x0039 }, // CapsLock
    Key { usb: 0x07003a, win: 0x003b, evdev: 0x003b, mac: 0x007a }, // F1
    Key { usb: 0x07003b, win: 0x003c, evdev: 0x003c, mac: 0x0078 }, // F2
    Key { usb: 0x07003c, win: 0x003d, evdev: 0x003d, mac: 0x0063 }, // F3
    Key { usb: 0x07003d, win: 0x003e, evdev: 0x003e, mac: 0x0076 }, // F4
    Key { usb: 0x07003e, win: 0x003f, evdev: 0x003f, mac: 0x0060 }, // F5
    Key { usb: 0x07003f, win: 0x0040, evdev: 0x0040, mac: 0x0061 }, // F6
    Key { usb: 0x070040, win: 0x0041, evdev: 0x0041, mac: 0x0062 }, // F7
    Key { usb: 0x070041, win: 0x0042, evdev: 0x0042, mac: 0x0064 }, // F8
    Key { usb: 0x070042, win: 0x0043, evdev: 0x0043, mac: 0x0065 }, // F9
    Key { usb: 0x070043, win: 0x0044, evdev: 0x0044, mac: 0x006d }, // F10
    Key { usb: 0x070044, win: 0x0057, evdev: 0x0057, mac: 0x0067 }, // F11
    Key { usb: 0x070045, win: 0x0058, evdev: 0x0058, mac: 0x006f }, // F12
    Key { usb: 0x070046, win: 0xe037, evdev: 0x0063, mac: 0xffff }, // PrintScreen
    Key { usb: 0x070047, win: 0x0046, evdev: 0x0046, mac: 0xffff }, // ScrollLock
    Key { usb: 0x070048, win: 0x0045, evdev: 0x0077, mac: 0xffff }, // Pause
    Key { usb: 0x070049, win: 0xe052, evdev: 0x006e, mac: 0x0072 }, // Insert
    Key { usb: 0x07004a, win: 0xe047, evdev: 0x0066, mac: 0x0073 }, // Home
    Key { usb: 0x07004b, win: 0xe049, evdev: 0x0068, mac: 0x0074 }, // PageUp
    Key { usb: 0x07004c, win: 0xe053, evdev: 0x006f, mac: 0x0075 }, // Delete
    Key { usb: 0x07004d, win: 0xe04f, evdev: 0x006b, mac: 0x0077 }, // End
    Key { usb: 0x07004e, win: 0xe051, evdev: 0x006d, mac: 0x0079 }, // PageDown
    Key { usb: 0x07004f, win: 0xe04d, evdev: 0x006a, mac: 0x007c }, // ArrowRight
    Key { usb: 0x070050, win: 0xe04b, evdev: 0x0069, mac: 0x007b }, // ArrowLeft
    Key { usb: 0x070051, win: 0xe050, evdev: 0x006c, mac: 0x007d }, // ArrowDown
    Key { usb: 0x070052, win: 0xe048, evdev: 0x0067, mac: 0x007e }, // ArrowUp
    Key { usb: 0x070053, win: 0xe045, evdev: 0x0045, mac: 0x0047 }, // NumLock
    Key { usb: 0x070054, win: 0xe035, evdev: 0x0062, mac: 0x004b }, // NumpadDivide
    Key { usb: 0x070057, win: 0x004e, evdev: 0x004e, mac: 0x0045 }, // NumpadAdd
    Key { usb: 0x070058, win: 0xe01c, evdev: 0x0060, mac: 0x004c }, // NumpadEnter
    Key { usb: 0x070059, win: 0x004f, evdev: 0x004f, mac: 0x0053 }, // Numpad1
    Key { usb: 0x07005a, win: 0x0050, evdev: 0x0050, mac: 0x0054 }, // Numpad2
    Key { usb: 0x07005b, win: 0x0051, evdev: 0x0051, mac: 0x0055 }, // Numpad3
    Key { usb: 0x07005c, win: 0x004b, evdev: 0x004b, mac: 0x0056 }, // Numpad4
    Key { usb: 0x07005d, win: 0x004c, evdev: 0x004c, mac: 0x0057 }, // Numpad5
    Key { usb: 0x07005e, win: 0x004d, evdev: 0x004d, mac: 0x0058 }, // Numpad6
    Key { usb: 0x07005f, win: 0x0047, evdev: 0x0047, mac: 0x0059 }, // Numpad7
    Key { usb: 0x070060, win: 0x0048, evdev: 0x0048, mac: 0x005b }, // Numpad8
    Key { usb: 0x070061, win: 0x0049, evdev: 0x0049, mac: 0x005c }, // Numpad9
    Key { usb: 0x070062, win: 0x0052, evdev: 0x0052, mac: 0x0052 }, // Numpad0
    Key { usb: 0x070064, win: 0x0056, evdev: 0x0056, mac: 0x000a }, // IntlBackslash
    Key { usb: 0x070065, win: 0xe05d, evdev: 0x007f, mac: 0x006e }, // ContextMenu
    Key { usb: 0x070066, win: 0xe05e, evdev: 0x0074, mac: 0xffff }, // Power
    Key { usb: 0x070067, win: 0x0059, evdev: 0x0075, mac: 0x0051 }, // NumpadEqual
    Key { usb: 0x070068, win: 0x0064, evdev: 0x00b7, mac: 0x0069 }, // F13
    Key { usb: 0x070069, win: 0x0065, evdev: 0x00b8, mac: 0x006b }, // F14
    Key { usb: 0x07006a, win: 0x0066, evdev: 0x00b9, mac: 0x0071 }, // F15
    Key { usb: 0x07006b, win: 0x0067, evdev: 0x00ba, mac: 0x006a }, // F16
    Key { usb: 0x07006c, win: 0x0068, evdev: 0x00bb, mac: 0x0040 }, // F17
    Key { usb: 0x07006d, win: 0x0069, evdev: 0x00bc, mac: 0x004f }, // F18
    Key { usb: 0x07006e, win: 0x006a, evdev: 0x00bd, mac: 0x0050 }, // F19
    Key { usb: 0x07006f, win: 0x006b, evdev: 0x00be, mac: 0x005a }, // F20
    Key { usb: 0x070070, win: 0x006c, evdev: 0x00bf, mac: 0xffff }, // F21
    Key { usb: 0x070071, win: 0x006d, evdev: 0x00c0, mac: 0xffff }, // F22
    Key { usb: 0x070072, win: 0x006e, evdev: 0x00c1, mac: 0xffff }, // F23
    Key { usb: 0x070073, win: 0x0076, evdev: 0x00c2, mac: 0xffff }, // F24
    Key { usb: 0x070074, win: 0x0000, evdev: 0x0086, mac: 0xffff }, // Open
    Key { usb: 0x070075, win: 0xe03b, evdev: 0x008a, mac: 0xffff }, // Help
    Key { usb: 0x070077, win: 0x0000, evdev: 0x0084, mac: 0xffff }, // Select
    Key { usb: 0x070079, win: 0x0000, evdev: 0x0081, mac: 0xffff }, // Again
    Key { usb: 0x07007a, win: 0xe008, evdev: 0x0083, mac: 0xffff }, // Undo
    Key { usb: 0x07007b, win: 0xe017, evdev: 0x0089, mac: 0xffff }, // Cut
    Key { usb: 0x07007c, win: 0xe018, evdev: 0x0085, mac: 0xffff }, // Copy
    Key { usb: 0x07007d, win: 0xe00a, evdev: 0x0087, mac: 0xffff }, // Paste
    Key { usb: 0x07007e, win: 0x0000, evdev: 0x0088, mac: 0xffff }, // Find
    Key { usb: 0x07007f, win: 0xe020, evdev: 0x0071, mac: 0x004a }, // AudioVolumeMute
    Key { usb: 0x070080, win: 0xe030, evdev: 0x0073, mac: 0x0048 }, // AudioVolumeUp
    Key { usb: 0x070081, win: 0xe02e, evdev: 0x0072, mac: 0x0049 }, // AudioVolumeDown
    Key { usb: 0x070085, win: 0x007e, evdev: 0x0079, mac: 0x005f }, // NumpadComma
    Key { usb: 0x070087, win: 0x0073, evdev: 0x0059, mac: 0x005e }, // IntlRo
    Key { usb: 0x070088, win: 0x0070, evdev: 0x005d, mac: 0xffff }, // KanaMode
    Key { usb: 0x070089, win: 0x007d, evdev: 0x007c, mac: 0x005d }, // IntlYen
    Key { usb: 0x07008a, win: 0x0079, evdev: 0x005c, mac: 0xffff }, // Convert
    Key { usb: 0x07008b, win: 0x007b, evdev: 0x005e, mac: 0xffff }, // NonConvert
    Key { usb: 0x070090, win: 0x0072, evdev: 0x007a, mac: 0x0068 }, // Lang1
    Key { usb: 0x070091, win: 0x0071, evdev: 0x007b, mac: 0x0066 }, // Lang2
    Key { usb: 0x070092, win: 0x0078, evdev: 0x005a, mac: 0xffff }, // Lang3
    Key { usb: 0x070093, win: 0x0077, evdev: 0x005b, mac: 0xffff }, // Lang4
    Key { usb: 0x070094, win: 0x0000, evdev: 0x0055, mac: 0xffff }, // Lang5
    Key { usb: 0x0700e0, win: 0x001d, evdev: 0x001d, mac: 0x003b }, // ControlLeft
    Key { usb: 0x0700e1, win: 0x002a, evdev: 0x002a, mac: 0x0038 }, // ShiftLeft
    Key { usb: 0x0700e2, win: 0x0038, evdev: 0x0038, mac: 0x003a }, // AltLeft
    Key { usb: 0x0700e3, win: 0xe05b, evdev: 0x007d, mac: 0x0037 }, // MetaLeft
    Key { usb: 0x0700e4, win: 0xe01d, evdev: 0x0061, mac: 0x003e }, // ControlRight
    Key { usb: 0x0700e5, win: 0x0036, evdev: 0x0036, mac: 0x003c }, // ShiftRight
    Key { usb: 0x0700e6, win: 0xe038, evdev: 0x0064, mac: 0x003d }, // AltRight
    Key { usb: 0x0700e7, win: 0xe05c, evdev: 0x007e, mac: 0x0036 }, // MetaRight
    Key { usb: 0x0c006f, win: 0x0000, evdev: 0x00e1, mac: 0xffff }, // BrightnessUp
    Key { usb: 0x0c00b0, win: 0x0000, evdev: 0x00cf, mac: 0xffff }, // MediaPlay
    Key { usb: 0x0c00b1, win: 0x0000, evdev: 0x00c9, mac: 0xffff }, // MediaPause
    Key { usb: 0x0c00b2, win: 0x0000, evdev: 0x00a7, mac: 0xffff }, // MediaRecord
    Key { usb: 0x0c00b3, win: 0x0000, evdev: 0x00d0, mac: 0xffff }, // MediaFastForward
    Key { usb: 0x0c00b4, win: 0x0000, evdev: 0x00a8, mac: 0xffff }, // MediaRewind
    Key { usb: 0x0c00b7, win: 0xe024, evdev: 0x00a6, mac: 0xffff }, // MediaStop
    Key { usb: 0x0c00b8, win: 0xe02c, evdev: 0x00a1, mac: 0xffff }, // Eject
    Key { usb: 0x0c0183, win: 0xe06d, evdev: 0x00ab, mac: 0xffff }, // MediaSelect
    Key { usb: 0x0c018a, win: 0xe06c, evdev: 0x009b, mac: 0xffff }, // LaunchMail
    Key { usb: 0x0c0192, win: 0xe021, evdev: 0x008c, mac: 0xffff }, // LaunchApp2
    Key { usb: 0x0c0194, win: 0xe06b, evdev: 0x0090, mac: 0xffff }, // LaunchApp1
    Key { usb: 0x0c01a2, win: 0x0000, evdev: 0x0244, mac: 0xffff }, // SelectTask
    Key { usb: 0x0c0221, win: 0xe065, evdev: 0x00d9, mac: 0xffff }, // BrowserSearch
    Key { usb: 0x0c0223, win: 0xe032, evdev: 0x00ac, mac: 0xffff }, // BrowserHome
    Key { usb: 0x0c0224, win: 0xe06a, evdev: 0x009e, mac: 0xffff }, // BrowserBack
    Key { usb: 0x0c0226, win: 0xe068, evdev: 0x0080, mac: 0xffff }, // BrowserStop
    Key { usb: 0x0c0289, win: 0x0000, evdev: 0x00e8, mac: 0xffff }, // MailReply
    Key { usb: 0x0c028b, win: 0x0000, evdev: 0x00e9, mac: 0xffff }, // MailForward
    Key { usb: 0x0c028c, win: 0x0000, evdev: 0x00e7, mac: 0xffff }, // MailSend
];
