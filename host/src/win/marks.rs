// The marks of the viewers on Windows: a layered window of each stroke, over all the others, that the mouse goes through
// ( WS_EX_LAYERED | WS_EX_TRANSPARENT ), never active. Its image is premultiplied BGRA ( UpdateLayeredWindow ); to fade, only its
// opacity changes - the image stays where Windows keeps it. The capture of the desktop sees it as the user does.

use std::ffi::c_void;
use std::sync::OnceLock;

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION,
    DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, PeekMessageW, RegisterClassW, SetWindowPos, ShowWindow, UpdateLayeredWindow, HWND_TOPMOST, MSG,
    PM_REMOVE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_SHOWNOACTIVATE, ULW_ALPHA, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};

use super::video::Display;

/// How large the marks are on a display of `size`: one of 1080 lines is 1.
pub fn scale(size: (u32, u32)) -> f32 { (size.0.min(size.1) as f32 / 1080.0).max(1.0) }

/// The order of the bytes of an image: B, G, R, A.
pub const RGBA: bool = false;

/// The window of one stroke, with the bitmap it shows.
pub struct Layer {
    window: HWND,
    dc:     HDC,
    bitmap: HBITMAP,
    old:    HGDIOBJ,
    bits:   *mut u8,
    size:   (u32, u32),
    shown:  bool,
}

impl Layer {
    /// A new layer, not shown yet; Err - why there is none.
    pub fn new() -> Result<Self, String> {
        static CLASS: OnceLock<()> = OnceLock::new();
        unsafe {
            CLASS.get_or_init(|| {
                let class = WNDCLASSW { lpfnWndProc: Some(window_proc), lpszClassName: w!("RemoteDeskMark"), ..Default::default() };
                RegisterClassW(&class);
            });
            let window = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("RemoteDeskMark"),
                w!(""),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                None,
                None,
            )
            .map_err(|e| format!("no window over the screen: {e}"))?;
            let dc = CreateCompatibleDC(None);
            Ok(Self { window, dc, bitmap: HBITMAP::default(), old: HGDIOBJ::default(), bits: std::ptr::null_mut(), size: (0, 0), shown: false })
        }
    }

    /// Shows `pixels`, `width` × `height`, at `x`, `y` of `display`, of opacity `alpha`.
    pub fn show(&mut self, display: &Display, x: i32, y: i32, width: u32, height: u32, pixels: &[u8], alpha: f32) {
        unsafe {
            if self.size != (width, height) || self.bits.is_null() {
                self.free();
                let info = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: width as i32,
                        biHeight: -(height as i32), // from the top down, as the image is
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut bits: *mut c_void = std::ptr::null_mut();
                let Ok(bitmap) = CreateDIBSection(Some(self.dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) else { return };
                self.bitmap = bitmap;
                self.old = SelectObject(self.dc, HGDIOBJ(bitmap.0));
                self.bits = bits as *mut u8;
                self.size = (width, height);
            }
            let n = (width * height * 4) as usize;
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), self.bits, n.min(pixels.len()));
            let at = POINT { x: display.x + x, y: display.y + y };
            let size = SIZE { cx: width as i32, cy: height as i32 };
            let from = POINT { x: 0, y: 0 };
            let blend = blend(alpha);
            let _ = UpdateLayeredWindow(self.window, None, Some(&at), Some(&size), Some(self.dc), Some(&from), COLORREF(0), Some(&blend), ULW_ALPHA);
            if !self.shown {
                self.shown = true;
                let _ = ShowWindow(self.window, SW_SHOWNOACTIVATE);
            }
            // over the windows that came to the top since
            let _ = SetWindowPos(self.window, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }

    /// The opacity of what it shows.
    pub fn fade(&mut self, alpha: f32) {
        if !self.shown {
            return;
        }
        let blend = blend(alpha);
        unsafe {
            let _ = UpdateLayeredWindow(self.window, None, None, None, None, None, COLORREF(0), Some(&blend), ULW_ALPHA);
        }
    }

    fn free(&mut self) {
        unsafe {
            if !self.bitmap.is_invalid() {
                SelectObject(self.dc, self.old);
                let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
                self.bitmap = HBITMAP::default();
            }
        }
        self.bits = std::ptr::null_mut();
    }
}

impl Drop for Layer {
    fn drop(&mut self) {
        self.free();
        unsafe {
            let _ = DeleteDC(self.dc);
            let _ = DestroyWindow(self.window);
        }
    }
}

unsafe extern "system" fn window_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT { DefWindowProcW(window, message, wparam, lparam) }

fn blend(alpha: f32) -> BLENDFUNCTION {
    BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: (alpha.clamp(0.0, 1.0) * 255.0) as u8, AlphaFormat: AC_SRC_ALPHA as u8 }
}

/// The messages of the windows of the layers: on their thread, between the frames.
pub fn pump() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            DispatchMessageW(&msg);
        }
    }
}
