// The native library of encryption from the latest release of adhoc-crypto, next to the program, where it is looked for when a connection
// first needs it. Nothing links to it: a download that fails only warns, and what encrypts nothing runs without it. One there already is
// taken again only when the release has a newer one; the lib naming another version of it runs this again.
//
//   ADHOC_CRYPTO=off       nothing is fetched
//   ADHOC_CRYPTO_URL       another place to fetch from
use std::{env, fs, path::Path, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=protocol/lib/crypto.rs");
    println!("cargo:rerun-if-env-changed=ADHOC_CRYPTO");
    println!("cargo:rerun-if-env-changed=ADHOC_CRYPTO_URL");
    if env::var("ADHOC_CRYPTO").as_deref() == Ok("off") {
        return;
    }
    let var = |name: &str| env::var(name).unwrap_or_default();
    let (os, ext) = match (var("CARGO_CFG_TARGET_OS").as_str(), var("CARGO_CFG_TARGET_ENV").as_str()) {
        ("windows", _) => ("win", "dll"),
        ("macos", _) => ("osx", "dylib"),
        ("freebsd", _) => ("freebsd", "so"),
        ("linux", "musl") => ("linux-musl", "so"),
        ("linux", _) => ("linux", "so"),
        _ => return,
    };
    let arch = match var("CARGO_CFG_TARGET_ARCH").as_str() {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "x86",
        "arm" => "arm",
        "riscv64" => "riscv64",
        _ => return,
    };
    // <target>/<profile>/build/<package>/out, or deeper in a newer layout of cargo: the program is in the folder above `build`
    let out = var("OUT_DIR");
    let Some(program) = Path::new(&out).ancestors().find(|p| p.file_name().is_some_and(|n| n == "build")).and_then(Path::parent) else { return };
    let file = program.join(format!("libadhoc-crypto-0.1.{ext}"));
    let from = env::var("ADHOC_CRYPTO_URL").unwrap_or_else(|_| String::from("https://github.com/AdHoc-Protocol/adhoc-crypto/releases/latest/download"));
    let url = format!("{from}/adhoc-crypto-{os}-{arch}.{ext}");
    if file.exists() {
        let fresh = program.join(format!("libadhoc-crypto-0.1.{ext}.new"));
        let newer = Command::new("curl").args(["-fsSL", "--connect-timeout", "20", "-R", "-z"]).arg(&file).arg("-o").arg(&fresh).arg(&url).status().is_ok_and(|status| status.success());
        if newer && fs::metadata(&fresh).is_ok_and(|m| 0 < m.len()) {
            let _ = fs::rename(&fresh, &file);
        }
        let _ = fs::remove_file(&fresh);
        return;
    }
    // a network that drops a moment is tried again; what curl said goes into the warning
    let fetched = Command::new("curl").args(["-fsSL", "--retry", "3", "--retry-delay", "2", "--connect-timeout", "20", "-o"]).arg(&file).arg(&url).output();
    if !fetched.as_ref().is_ok_and(|o| o.status.success()) {
        let _ = fs::remove_file(&file);
        let said = fetched.map(|o| String::from_utf8_lossy(&o.stderr).trim().replace('\n', " ")).unwrap_or_else(|e| e.to_string());
        println!("cargo:warning=Encryption needs the native library, and it did not come from {url}: {said}");
    }
}
                         