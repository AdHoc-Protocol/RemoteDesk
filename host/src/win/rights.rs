// The rights of an administrator on Windows: the host is started again by `runas`, the user of the machine confirms in the prompt
// of UAC.

use std::sync::LazyLock;

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, SW_SHOWMINNOACTIVE};

/// Why the host was not started again.
pub enum Refused {
    /// The user of the machine said no.
    Declined,
    Failed(String),
}

/// The host runs as administrator.
static ELEVATED: LazyLock<bool> = LazyLock::new(|| unsafe { elevated_process(GetCurrentProcess()) });

pub fn elevated() -> bool { *ELEVATED }

/// The rights matter: a window of a program run as administrator takes no input of a program that is not.
pub fn matter() -> bool { true }

/// Starts the host again as administrator with `args`: returns once the user of the machine answered the prompt of UAC.
pub fn relaunch(args: &[String]) -> Result<(), Refused> {
    let line = args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
    let exe = std::env::current_exe().map(|p| HSTRING::from(p.as_os_str())).unwrap_or_default();
    let parameters = HSTRING::from(line);
    let verb = HSTRING::from("runas");
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC,
        hwnd: unsafe { GetForegroundWindow() }, // the prompt comes to the front, not to a button of the taskbar
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(exe.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        nShow: SW_SHOWMINNOACTIVE.0,
        ..Default::default()
    };
    match unsafe { ShellExecuteExW(&mut info) } {
        Ok(()) => Ok(()),
        Err(e) if e.code().0 as u32 & 0xFFFF == 1223 => Err(Refused::Declined), // ERROR_CANCELLED
        Err(e) => Err(Refused::Failed(e.to_string())),
    }
}

/// An argument as the command line of Windows reads it back: quoted when it has a space or a quote, backslashes before a quote
/// doubled.
fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::from('"');
    let mut slashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
                out.push('"');
                slashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', slashes));
                slashes = 0;
                out.push(c);
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', slashes * 2));
    out.push('"');
    out
}

unsafe fn elevated_process(process: HANDLE) -> bool {
    let mut token = HANDLE::default();
    if OpenProcessToken(process, TOKEN_QUERY, &mut token).is_err() {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION::default();
    let mut size = 0u32;
    let ok = GetTokenInformation(token, TokenElevation, Some(&mut elevation as *mut _ as *mut _), std::mem::size_of::<TOKEN_ELEVATION>() as u32, &mut size).is_ok();
    let _ = CloseHandle(token);
    ok && elevation.TokenIsElevated != 0
}

#[cfg(test)]
mod tests {
    use super::quote;

    #[test]
    fn quoted_as_windows_reads_back() {
        assert_eq!(quote("--token"), "--token");
        assert_eq!(quote(r"D:\My Keys\host.key"), r#""D:\My Keys\host.key""#);
        assert_eq!(quote(r"C:\dir with space\"), r#""C:\dir with space\\""#);
        assert_eq!(quote(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote(r#"x\"y z"#), r#""x\\\"y z""#);
        assert_eq!(quote(""), r#""""#);
    }
}
