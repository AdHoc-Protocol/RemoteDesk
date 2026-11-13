// Copyright 2025 Chikirev Sirguy, Unirail Group
// Licensed under the Apache License, Version 2.0.

//! The native library adhoc-crypto: found and loaded at the first use, so that a host with no
//! encryption neither needs it to build nor to run.
//!
//! Looked for in `ADHOC_CRYPTO_NATIVE` (the path of the file), next to the program, and in
//! `lib/native/<platform>/` of the crate the runtime was built in. The calls take addresses as `i64`,
//! as the header `adhoc_crypto.h` has them.

use std::ffi::{c_char, c_void};
use std::sync::OnceLock;

/// What this code expects of the library ([`Api::abi`]).
const ABI: i32 = 10;
const VERSION: &str = "0.1";

/// What a certificate check hears: the context, how many certificates, their DER and their lengths.
pub(crate) type Verify = unsafe extern "C" fn(*mut c_void, i32, *const *const u8, *const i32) -> i32;

macro_rules! api {
    ($($name:ident = $symbol:literal: fn($($a:ty),*) $(-> $r:ty)?;)*) => {
        /// The functions of the library.
        pub(crate) struct Api { $(pub $name: unsafe extern "C" fn($($a),*) $(-> $r)?,)* }

        impl Api {
            unsafe fn of(lib: *mut c_void) -> Result<Api, String> {
                Ok(Api { $($name: std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($a),*) $(-> $r)?>(symbol(lib, concat!("adhoc_", $symbol, "\0"))?),)* })
            }
        }
    };
}

api! {
    abi = "abi": fn() -> i32;
    aead_size = "aead_size": fn() -> i32;
    aead_init = "aead_init": fn(i32, *mut u8, *const u8, i32) -> i32;
    seal = "seal": fn(i64, i64, i64, i32, i64, i32) -> i32;
    open = "open": fn(i64, i64, i64, i32, i64, i32) -> i32;
    seal_nonce = "seal_nonce": fn(i64, *const u8, *const u8, i32, *mut u8, i32) -> i32;
    open_nonce = "open_nonce": fn(i64, *const u8, *const u8, i32, *mut u8, i32) -> i32;
    seal_records = "seal_records": fn(i64, i64, i64, i32) -> i64;
    open_records = "open_records": fn(i64, i64, i64, i32, i32) -> i64;
    stream_xor = "stream_xor": fn(i64, i64, i32);
    x25519 = "x25519": fn(*mut u8, *const u8, *const u8) -> i32;
    x25519_public = "x25519_public": fn(*mut u8, *const u8);
    x25519_keypair = "x25519_keypair": fn(*mut u8, *mut u8);
    noise_size = "noise_size": fn() -> i32;
    noise_tx = "noise_tx": fn() -> i32;
    noise_rx = "noise_rx": fn() -> i32;
    noise_init = "noise_init": fn(i64, i32, i32, *const u8, i32, *const u8, *const u8, *const u8, *const u8);
    noise_write = "noise_write": fn(i64, *mut u8, i32, *const u8, i32) -> i32;
    noise_read = "noise_read": fn(i64, *const u8, i32, *mut u8, i32) -> i32;
    tls_context = "tls_context": fn() -> i64;
    tls_server_context = "tls_server_context": fn(*const u8, i32, *const u8, i32) -> i64;
    tls_server = "tls_server": fn(i64) -> i64;
    tls_client = "tls_client": fn(i64, *const c_char, *const u8, i32, Verify, *mut c_void) -> i64;
    tls_handshake = "tls_handshake": fn(i64, i64, i32, i64, i32) -> i64;
    tls_tx_limit = "tls_tx_limit": fn(i64, i64);
    tls_control = "tls_control": fn(i64, i64) -> i32;
    tls_close_notify = "tls_close_notify": fn(i64);
    tls_error = "tls_error": fn(i64) -> *const c_char;
    tls_free = "tls_free": fn(i64);
    chain_size = "chain_size": fn() -> i32;
    chain_tx = "chain_tx": fn(i64) -> i32;
    chain_rx = "chain_rx": fn(i64, *mut u8, i32, i32) -> i64;
    ed25519_keypair = "ed25519_keypair": fn(*mut u8, *mut u8, *const u8);
    ed25519_sign = "ed25519_sign": fn(*mut u8, *const u8, *const u8, i64) -> i32;
    ed25519_verify = "ed25519_verify": fn(*const u8, *const u8, *const u8, i64) -> i32;
    pair_key = "pair_key": fn(*mut u8, *mut u8, *mut i32, *const u8, *const u8, *const u8, i32) -> i32;
    pack_seal = "pack_seal": fn(i64, i64, i64, i64, i32, i32) -> i32;
    pack_open = "pack_open": fn(i64, i64, i64, i64, i32, i32) -> i32;
    stages_size = "stages_size": fn() -> i32;
    stages_tx = "stages_tx": fn(i64, *mut u8, i32, i32) -> i32;
    stages_rx = "stages_rx": fn(i64, *mut u8, i32, i32) -> i32;
    stages_xor = "stages_xor": fn(i64, *mut u8, i32);
    stages_free = "stages_free": fn(i64);
}

#[cfg(windows)]
mod os {
    use std::ffi::{c_char, c_void};
    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryA(name: *const c_char) -> *mut c_void;
        fn GetProcAddress(lib: *mut c_void, name: *const c_char) -> *mut c_void;
    }
    pub(super) unsafe fn open(path: *const c_char) -> *mut c_void {
        LoadLibraryA(path)
    }
    pub(super) unsafe fn symbol(lib: *mut c_void, name: *const c_char) -> *mut c_void {
        GetProcAddress(lib, name)
    }
    pub(super) const FILE: &str = "dll";
    pub(super) const OS: &str = "win";
}

#[cfg(unix)]
mod os {
    use std::ffi::{c_char, c_void};
    extern "C" {
        fn dlopen(name: *const c_char, flags: i32) -> *mut c_void;
        fn dlsym(lib: *mut c_void, name: *const c_char) -> *mut c_void;
    }
    pub(super) unsafe fn open(path: *const c_char) -> *mut c_void {
        dlopen(path, 2) // RTLD_NOW
    }
    pub(super) unsafe fn symbol(lib: *mut c_void, name: *const c_char) -> *mut c_void {
        dlsym(lib, name)
    }
    pub(super) const FILE: &str = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    pub(super) const OS: &str = if cfg!(target_os = "macos") {
        "osx"
    } else if cfg!(target_os = "freebsd") {
        "freebsd"
    } else if cfg!(target_env = "musl") {
        "linux-musl" // a library built for glibc does not load on it, and the other way round
    } else {
        "linux"
    };
}

/// The processor in the name of a folder of the library.
const ARCH: &str = if cfg!(target_arch = "x86_64") {
    "x64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    std::env::consts::ARCH // x86, arm, riscv64: as they are named
};

unsafe fn symbol(lib: *mut c_void, name: &str) -> Result<*mut c_void, String> {
    let f = os::symbol(lib, name.as_ptr() as *const c_char);
    if f.is_null() {
        return Err(format!("the native library adhoc-crypto has no {}: take the one of this release", &name[..name.len() - 1]));
    }
    Ok(f)
}

/// Opens the library at `path`, when it is there.
pub(crate) fn open_library(path: &std::path::Path) -> *mut c_void {
    match std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
        Ok(c) => unsafe { os::open(c.as_ptr()) },
        Err(_) => std::ptr::null_mut(),
    }
}

/// A function of a library opened by [`open_library`]: null - it has none.
pub(crate) unsafe fn library_symbol(lib: *mut c_void, name: &'static str) -> *mut c_void {
    os::symbol(lib, name.as_ptr() as *const c_char)
}

fn load() -> Result<Api, String> {
    let file = format!("libadhoc-crypto-{VERSION}.{}", os::FILE);
    let platform = format!("{}-{ARCH}", os::OS);
    let mut paths: Vec<std::path::PathBuf> = Vec::with_capacity(4);
    if let Some(p) = std::env::var_os("ADHOC_CRYPTO_NATIVE") {
        paths.push(p.into());
    }
    if let Some(dir) = std::env::current_exe().ok().as_deref().and_then(std::path::Path::parent) {
        paths.push(dir.join(&file));
        // a test of cargo runs from <target>/<profile>/deps: build.rs puts the library one folder up, next to the program
        if dir.file_name().is_some_and(|name| name == "deps") {
            if let Some(program) = dir.parent() {
                paths.push(program.join(&file));
            }
        }
    }
    if let Some(root) = option_env!("CARGO_MANIFEST_DIR") {
        paths.push(std::path::Path::new(root).join("lib").join("native").join(&platform).join(&file));
    }
    for path in &paths {
        let lib = open_library(path);
        if lib.is_null() {
            continue;
        }
        // SAFETY: the signatures are those of adhoc_crypto.h
        let api = unsafe { Api::of(lib)? };
        if unsafe { (api.abi)() } != ABI {
            return Err(format!("the native library {} does not match this lib: take the one of its release", path.display()));
        }
        return Ok(api);
    }
    Err(format!("encryption needs the native library {file} of the platform {platform}: build.rs takes it, or download https://github.com/AdHoc-Protocol/adhoc-crypto/releases/latest/download/adhoc-crypto-{platform}.{} and put it next to the program under that name, or name it in ADHOC_CRYPTO_NATIVE", os::FILE))
}

static API: OnceLock<Result<Api, String>> = OnceLock::new();

/// The library, loaded with the first call: what is missing, when it cannot be.
pub(crate) fn try_api() -> Result<&'static Api, &'static str> {
    match API.get_or_init(load) {
        Ok(a) => Ok(a),
        Err(e) => Err(e),
    }
}

/// The library. Panics with what is missing: anything encrypted reaches here first.
#[inline]
pub(crate) fn api() -> &'static Api {
    match API.get() {
        Some(Ok(a)) => a,
        _ => try_api().unwrap_or_else(|e| panic!("{e}")),
    }
}

/// Native memory of a fixed place: aligned to 16 as the library asks, zeroed. Reached from the
/// library by its address, so it never moves.
pub(crate) struct Mem {
    ptr: *mut u8,
    layout: std::alloc::Layout,
}

// SAFETY: plain bytes; who shares them says who touches them when
unsafe impl Send for Mem {}
unsafe impl Sync for Mem {}

impl Mem {
    pub(crate) fn new(size: usize) -> Self {
        let layout = std::alloc::Layout::from_size_align(size, 16).expect("a size of native memory");
        // SAFETY: the size is not zero
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Self { ptr, layout }
    }

    #[inline(always)]
    pub(crate) fn ptr(&self) -> *mut u8 {
        self.ptr
    }

    #[inline(always)]
    pub(crate) fn at(&self) -> i64 {
        self.ptr as i64
    }

    #[inline(always)]
    pub(crate) fn len(&self) -> usize {
        self.layout.size()
    }
}

impl Drop for Mem {
    fn drop(&mut self) {
        // SAFETY: as allocated
        unsafe { std::alloc::dealloc(self.ptr, self.layout) };
    }
}

/// A signing pair of Ed25519: 64 bytes of secret, 32 of public. `seed`: 32 bytes it comes from;
/// `None` - random.
pub fn ed25519_keypair(seed: Option<&[u8; 32]>) -> ([u8; 64], [u8; 32]) {
    let (mut secret, mut public) = ([0u8; 64], [0u8; 32]);
    // SAFETY: the sizes the call takes
    unsafe { (api().ed25519_keypair)(secret.as_mut_ptr(), public.as_mut_ptr(), seed.map_or(std::ptr::null(), |s| s.as_ptr())) };
    (secret, public)
}

/// The signature of `message`, 64 bytes: Ed25519ph of RFC 8032 §5.1, with no context.
pub fn ed25519_sign(secret: &[u8; 64], message: &[u8]) -> [u8; 64] {
    let mut signature = [0u8; 64];
    // SAFETY: the sizes the call takes
    unsafe { (api().ed25519_sign)(signature.as_mut_ptr(), secret.as_ptr(), message.as_ptr(), message.len() as i64) };
    signature
}

pub fn ed25519_verify(signature: &[u8; 64], public: &[u8; 32], message: &[u8]) -> bool {
    // SAFETY: the sizes the call takes
    unsafe { (api().ed25519_verify)(signature.as_ptr(), public.as_ptr(), message.as_ptr(), message.len() as i64) != 0 }
}
