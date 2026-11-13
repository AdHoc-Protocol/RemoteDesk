// Makes the key tables of RemoteDesk from Chromium's ui/events/keycodes/dom/dom_code_data.inc ( BSD license, beside this file ):
//   viewer/src/keymap.ts - KeyboardEvent.code → USB HID usage: what the page sends;
//   host/src/keymap.rs   - USB HID usage → the code of the key on Windows, Linux ( evdev ) and macOS: what the machine injects.
// Run: node keymap/gen.mjs   ( from D:\rdp )

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const source = readFileSync(join(here, "dom_code_data.inc"), "utf8");

// DOM_CODE(usb, evdev, xkb, win, mac, "code", id)
const keys = [];
for (const line of source.split("\n")) {
    const m = /^\s*DOM_CODE\((0x[0-9a-fA-F]+),\s*(0x[0-9a-fA-F]+),\s*0x[0-9a-fA-F]+,\s*(0x[0-9a-fA-F]+),\s*(0x[0-9a-fA-F]+),\s*"([^"]*)",\s*(\w+)\)/.exec(line);
    if (!m) continue;
    keys.push({ usb: parseInt(m[1], 16), evdev: parseInt(m[2], 16), win: parseInt(m[3], 16), mac: parseInt(m[4], 16), code: m[5], id: m[6] });
}

const hex = (n, digits) => "0x" + n.toString(16).padStart(digits, "0");
const header = "Made by keymap/gen.mjs from Chromium's ui/events/keycodes/dom/dom_code_data.inc: do not edit, run it again.";

const ts = keys.filter(k => k.code && k.usb);
writeFileSync(join(here, "..", "viewer", "src", "keymap.ts"),
    `// ${header}\n\n` +
    `/** KeyboardEvent.code → USB HID usage: the page in the upper 16 bits, the usage in the lower. */\n` +
    `export const usbOf: Readonly<Record<string, number>> = {\n` +
    ts.map(k => `    ${JSON.stringify(k.code)}: ${hex(k.usb, 6)},`).join("\n") +
    `\n};\n`);

// the keys the machine can inject on one system at least, by their USB HID usage ascending: looked up by a binary search
const rs = keys.filter(k => k.usb && (k.win || k.evdev || k.mac !== 0xffff)).sort((a, b) => a.usb - b.usb);
writeFileSync(join(here, "..", "host", "src", "keymap.rs"),
    `// ${header}\n\n` +
    `/// A key: its USB HID usage and its codes on the systems. 0 ( mac: 0xFFFF ) - the system has no such key.\n` +
    `#[derive(Clone, Copy, Debug)]\n` +
    `#[allow(dead_code)] // each system reads its own code\n` +
    `pub struct Key {\n` +
    `    pub usb:   u32,\n` +
    `    /// Windows scan code: 0xE0xx - an extended key.\n` +
    `    pub win:   u16,\n` +
    `    /// Linux evdev code: the X11 keycode is 8 more.\n` +
    `    pub evdev: u16,\n` +
    `    /// macOS virtual key code.\n` +
    `    pub mac:   u16,\n` +
    `}\n\n` +
    `/// The key of a USB HID usage.\n` +
    `pub fn key(usb: u32) -> Option<&'static Key> { KEYS.binary_search_by_key(&usb, |k| k.usb).ok().map(|i| &KEYS[i]) }\n\n` +
    `/// Every key, by USB HID usage ascending.\n` +
    `pub const KEYS: &[Key] = &[\n` +
    rs.map(k => `    Key { usb: ${hex(k.usb, 6)}, win: ${hex(k.win, 4)}, evdev: ${hex(k.evdev, 4)}, mac: ${hex(k.mac, 4)} }, // ${k.code || k.id}`).join("\n") +
    `\n];\n`);

console.log(`keymap.ts: ${ts.length} keys, keymap.rs: ${rs.length} keys`);
