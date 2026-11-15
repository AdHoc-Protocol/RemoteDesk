// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! Transport failures and their reporting context.
//!
//! A host reports socket errors, protocol failures, callback panics, and journal errors to its
//! failure handler. I/O loops report their own failures to the engine handler.

use std::sync::{Arc, LazyLock};

/// The source - a connection, a host, a loop - and the failure.
pub type OnFailure = Arc<dyn Fn(&dyn std::fmt::Display, &(dyn std::error::Error + 'static)) + Send + Sync>;

/// The handler of a host given none: the source, then the failure, to the console.
pub fn print_console() -> OnFailure {
    static PRINT: LazyLock<OnFailure> = LazyLock::new(|| {
        Arc::new(|src: &dyn std::fmt::Display, e: &(dyn std::error::Error + 'static)| {
            println!("onFailure {src}");
            println!("{e}");
        })
    });
    PRINT.clone()
}

/// A panic as the failure it is: what it said, when it said it in text.
pub(crate) fn panicked(why: &(dyn std::any::Any + Send)) -> std::io::Error {
    let text = why.downcast_ref::<&str>().copied().or_else(|| why.downcast_ref::<String>().map(String::as_str)).unwrap_or("not text");
    std::io::Error::other(format!("panicked: {text}"))
}

/// A peer that breaks the protocol.
pub(crate) fn protocol(why: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, why.into())
}

/// A crash no one handled - an instruction the processor does not have, a wrong address -, in the
/// code of the program or in a library another program put into it ( a VPN, an antivirus ): on
/// Windows the process ends in silence, and a window opened by a double click closes. Caught, it
/// is told: what it was, the module and the offset in it, the calls that led there - to
/// `<program>-crash.log` beside the program, else in %TEMP%, and to the console, which waits for
/// Enter when it is of this program alone. A crash in a layered provider of Winsock ( an LSP ) is
/// said to be one: Windows puts it into every program that opens a socket, its own program running
/// or not, so only its removal helps. Set by the first [`super::engine::Engine`]; a program calls it
/// itself to have its start covered too. Elsewhere than on Windows: nothing.
pub fn catch_crashes() {
    #[cfg(windows)]
    {
        static SET: std::sync::Once = std::sync::Once::new();
        SET.call_once(|| unsafe {
            let _ = crash::LSP.set(crash::layered());
            crash::SetUnhandledExceptionFilter(Some(crash::crashed));
        });
    }
}

#[cfg(windows)]
mod crash {
    use std::ffi::c_void;
    use std::fmt::Write as _;
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[repr(C)]
    pub struct ExceptionRecord {
        code: u32,
        flags: u32,
        record: *mut ExceptionRecord,
        address: *mut c_void,
        parameters: u32,
        information: [usize; 15],
    }

    #[repr(C)]
    pub struct ExceptionPointers {
        record: *const ExceptionRecord,
        context: *const c_void,
    }

    #[repr(C)]
    struct OsVersion {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        service_pack: [u16; 128],
    }

    #[repr(C)]
    #[derive(Default)]
    struct SystemTime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }

    type Filter = unsafe extern "system" fn(*const ExceptionPointers) -> i32;

    #[link(name = "kernel32")]
    extern "system" {
        pub fn SetUnhandledExceptionFilter(filter: Option<Filter>) -> *mut c_void;
        fn GetModuleHandleExW(flags: u32, name: *const c_void, module: *mut *mut c_void) -> i32;
        fn GetModuleFileNameW(module: *mut c_void, name: *mut u16, size: u32) -> u32;
        fn RtlCaptureStackBackTrace(skip: u32, count: u32, frames: *mut *mut c_void, hash: *mut u32) -> u16;
        fn GetLocalTime(time: *mut SystemTime);
        fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
    }

    #[link(name = "ntdll")]
    extern "system" {
        fn RtlGetVersion(info: *mut OsVersion) -> i32;
    }

    /// An entry of the catalog of Winsock: WSAPROTOCOL_INFOW.
    #[repr(C)]
    struct ProtocolInfo {
        service_flags: [u32; 4],
        provider_flags: u32,
        provider_id: [u8; 16],
        catalog_entry_id: u32,
        /// 0 - a layered protocol, 1 - a base one, more - a chain over layered ones
        chain_len: i32,
        chain_entries: [u32; 7],
        numbers: [i32; 9],
        message_size: u32,
        provider_reserved: u32,
        protocol: [u16; 256],
    }

    #[link(name = "ws2_32")]
    extern "system" {
        fn WSAStartup(version: u16, data: *mut u8) -> i32;
        fn WSAEnumProtocolsW(protocols: *const i32, buffer: *mut ProtocolInfo, length: *mut u32) -> i32;
        fn WSCGetProviderPath(id: *const [u8; 16], path: *mut u16, length: *mut i32, error: *mut i32) -> i32;
    }

    /// The files of the layered providers of Winsock, in small letters: read at the start, not in a crash.
    pub static LSP: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

    /// The layered providers of the catalog of Winsock: the names of their files.
    pub fn layered() -> Vec<String> {
        let mut files: Vec<String> = Vec::new();
        unsafe {
            let mut data = [0u8; 512]; // WSADATA, with room
            if WSAStartup(0x0202, data.as_mut_ptr()) != 0 {
                return files;
            }
            let mut length = 0u32;
            WSAEnumProtocolsW(std::ptr::null(), std::ptr::null_mut(), &mut length); // the size it takes
            let count = length as usize / std::mem::size_of::<ProtocolInfo>() + 1;
            let mut all: Vec<ProtocolInfo> = Vec::with_capacity(count);
            length = (count * std::mem::size_of::<ProtocolInfo>()) as u32;
            let n = WSAEnumProtocolsW(std::ptr::null(), all.as_mut_ptr(), &mut length);
            if n <= 0 {
                return files;
            }
            all.set_len(n as usize);
            for p in all.iter().filter(|p| p.chain_len != 1) {
                let mut path = [0u16; 520];
                let mut len = path.len() as i32;
                let mut error = 0i32;
                if WSCGetProviderPath(&p.provider_id, path.as_mut_ptr(), &mut len, &mut error) != 0 {
                    continue;
                }
                let path = String::from_utf16_lossy(&path[..path.iter().position(|&c| c == 0).unwrap_or(path.len())]);
                let file = path.rsplit(['\\', '/']).next().unwrap_or(&path).to_ascii_lowercase();
                if !file.is_empty() && !files.contains(&file) {
                    files.push(file);
                }
            }
        }
        files
    }

    /// The first crash is told; a thread that crashes after it waits for the end.
    static TOLD: AtomicBool = AtomicBool::new(false);

    /// What the code of a crash means.
    fn named(code: u32) -> &'static str {
        match code {
            0xC000_001D => "an instruction this processor does not have",
            0xC000_0005 => "a wrong address of memory",
            0xC000_0096 => "a privileged instruction",
            0xC000_00FD => "the stack overflowed",
            0xC000_0094 => "an integer divided by zero",
            0xE06D_7363 => "an exception of C++",
            _ => "",
        }
    }

    /// The module `at` is in, and the offset in it; None - in no module.
    fn module_of(at: *const c_void) -> Option<(String, usize)> {
        let mut module: *mut c_void = std::ptr::null_mut();
        // FROM_ADDRESS | UNCHANGED_REFCOUNT
        if unsafe { GetModuleHandleExW(0x4 | 0x2, at, &mut module) } == 0 {
            return None;
        }
        let mut name = [0u16; 520];
        let n = unsafe { GetModuleFileNameW(module, name.as_mut_ptr(), name.len() as u32) } as usize;
        Some((String::from_utf16_lossy(&name[..n]), at as usize - module as usize))
    }

    pub unsafe extern "system" fn crashed(pointers: *const ExceptionPointers) -> i32 {
        if TOLD.swap(true, Ordering::AcqRel) {
            loop {
                std::thread::park(); // the first one tells, and ends the process
            }
        }
        let record = &*(*pointers).record;
        let at = record.address;
        let (module, offset) = module_of(at).unwrap_or_else(|| (String::from("no module"), at as usize));
        let thread = std::thread::current();
        let thread = thread.name().map(String::from).unwrap_or_else(|| format!("{:?}", thread.id()));
        let mut t = SystemTime::default();
        GetLocalTime(&mut t);
        let mut v = OsVersion { size: std::mem::size_of::<OsVersion>() as u32, major: 0, minor: 0, build: 0, platform: 0, service_pack: [0; 128] };
        RtlGetVersion(&mut v);
        let exe = std::env::current_exe().unwrap_or_default();

        let mut text = String::new();
        let _ = writeln!(text, "{}-{:02}-{:02} {:02}:{:02}:{:02}", t.year, t.month, t.day, t.hour, t.minute, t.second);
        let _ = writeln!(text, "{}, Windows {}.{} build {}", exe.display(), v.major, v.minor, v.build);
        let _ = writeln!(text, "The program crashed: {:#010x} {}, in {module} + {offset:#x}, the thread {thread}", record.code, named(record.code));
        if record.code == 0xC000_0005 && record.parameters >= 2 {
            let how = match record.information[0] {
                0 => "reading",
                1 => "writing",
                _ => "executing",
            };
            let _ = writeln!(text, "  {how} the address {:#x}", record.information[1]);
        }
        // of this program: what lies in its folder; else a layered provider of Winsock, of Windows, or of another program put into it
        let folder = exe.parent().map(|d| d.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        let lsp = LSP.get().map(Vec::as_slice).unwrap_or_default();
        let file = module.rsplit('\\').next().unwrap_or(&module).to_ascii_lowercase();
        if lsp.contains(&file) {
            let _ = writeln!(text, "  This library is a layered provider of Winsock ( an LSP ) another program installed: Windows puts it into every");
            let _ = writeln!(text, "  program that uses the network, whether that program runs or not. Turning it off does not help: remove its LSP,");
            let _ = writeln!(text, "  then restart the computer; the program goes on working without it. Astrill VPN: hold Ctrl + click its menu,");
            let _ = writeln!(text, "  Help, LSP Uninstall. No such item: netsh winsock reset, as administrator.");
        } else if folder.is_empty() || !module.to_ascii_lowercase().starts_with(&folder) {
            let _ = writeln!(text, "  This library is not of this program: of Windows, or of another program put into it - a VPN, an antivirus.");
            let _ = writeln!(text, "  If it is not of Windows, turn that program off, a VPN first, and start this one again.");
        }
        // the calls that led there, from the place of the crash on, each as its module and the offset in it
        let mut frames = [std::ptr::null_mut::<c_void>(); 64];
        let n = RtlCaptureStackBackTrace(0, frames.len() as u32, frames.as_mut_ptr(), std::ptr::null_mut()) as usize;
        let from = frames[..n].iter().position(|&f| f == at).unwrap_or(0);
        let _ = writeln!(text, "The calls:");
        for &f in &frames[from..n] {
            match module_of(f) {
                Some((m, o)) => {
                    let _ = writeln!(text, "  {m} + {o:#x}");
                }
                None => {
                    let _ = writeln!(text, "  {f:p}");
                }
            }
        }

        let name = format!("{}-crash.log", exe.file_stem().and_then(|s| s.to_str()).unwrap_or("program"));
        let mut written = None;
        for path in exe.parent().map(|d| d.join(&name)).into_iter().chain([std::env::temp_dir().join(&name)]) {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                if writeln!(f, "{text}").is_ok() {
                    written = Some(path);
                    break;
                }
            }
        }
        let mut err = std::io::stderr();
        let _ = writeln!(err, "\n ❌ {}", text.lines().nth(2).unwrap_or_default());
        for line in text.lines().skip(3).take_while(|l| l.starts_with("  ")) {
            let _ = writeln!(err, " {line}");
        }
        if let Some(path) = written {
            let _ = writeln!(err, " All of it is in {}", path.display());
        }
        // the console is of this process alone - opened for it by a double click, it closes with it: it stays to be read
        let mut list = [0u32; 2];
        if GetConsoleProcessList(list.as_mut_ptr(), 2) == 1 {
            let _ = writeln!(err, "\n Press Enter to close this window.");
            let _ = std::io::stdin().read_line(&mut String::new());
        }
        1 // EXCEPTION_EXECUTE_HANDLER: the process ends
    }
}
