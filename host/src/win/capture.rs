// The displays of the machine and the capture of one of them: DXGI Desktop Duplication.
//
// A frame comes when the desktop of the display changes; the pointer is not in it - its shape and position come beside it, the
// viewer draws the pointer itself.

use windows::core::{Interface, Result};
use windows::Win32::Foundation::{E_FAIL, HMODULE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270, DXGI_MODE_ROTATION_ROTATE90, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST,
    DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFOEXW};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

use crate::video::{Next, Pointer};

/// Coordinates of the desktop in physical pixels: the capture and the input agree on them.
pub fn dpi_aware() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// A display of the machine: its place in the virtual screen, in pixels.
#[derive(Clone, Debug)]
pub struct Display {
    pub screen:  u8,
    pub x:       i32,
    pub y:       i32,
    pub width:   u32,
    pub height:  u32,
    pub name:    String,
    pub primary: bool,
    pub adapter: IDXGIAdapter1,
    output:      IDXGIOutput1,
}

/// The displays attached to the desktop, numbered in the order DXGI gives them; 16 at most.
pub fn displays() -> Vec<Display> {
    let mut list = Vec::new();
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else { return list };
    let mut a = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(a) } {
        a += 1;
        let mut o = 0;
        loop {
            let output = match unsafe { adapter.EnumOutputs(o) } {
                Ok(output) => output,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(_) => break,
            };
            o += 1;
            let Ok(desc) = (unsafe { output.GetDesc() }) else { continue };
            if !desc.AttachedToDesktop.as_bool() {
                continue;
            }
            let Ok(output) = output.cast::<IDXGIOutput1>() else { continue };
            let r = desc.DesktopCoordinates;
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            let name_of_device = String::from_utf16_lossy(&desc.DeviceName).trim_end_matches('\0').to_string();
            let primary = unsafe { GetMonitorInfoW(desc.Monitor, &mut info.monitorInfo) }.as_bool() && info.monitorInfo.dwFlags & 1 != 0; // MONITORINFOF_PRIMARY
            list.push(Display {
                screen: list.len() as u8,
                x: r.left,
                y: r.top,
                width: (r.right - r.left) as u32,
                height: (r.bottom - r.top) as u32,
                name: name_of_device.trim_start_matches("\\\\.\\").to_string(),
                primary,
                adapter: adapter.clone(),
                output,
            });
            if list.len() == 16 {
                return list;
            }
        }
    }
    list
}

/// The capture of one display. On the GPU the frame stays a texture for the encoder there; else it is copied to a texture the CPU
/// reads.
pub struct Duplicator {
    pub display: Display,
    pub device:  ID3D11Device,
    pub context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    gpu:         bool,
    /// On the GPU: the texture of the frame acquired last, held until the next is acquired - the encoder reads it in place.
    held:        Option<ID3D11Texture2D>,
    /// What the frame is copied to for the CPU to read: the size of the texture DXGI gives - made at its first frame.
    staging:     Option<(ID3D11Texture2D, u32, u32)>,
    /// How the desktop is turned on the panel: DXGI gives the image as the panel shows it, the desktop is turned back.
    pub rotation: DXGI_MODE_ROTATION,
    /// The size of the desktop of the display, as it is turned.
    pub width:   u32,
    pub height:  u32,
    /// When the last frame was presented: QPC.
    pub time:    i64,
    /// For the CPU, the desktop of a turned panel turned back: BGRA, `width * 4` a row.
    bgra:        Vec<u8>,
    pub pointer: Pointer,
    shape:       Vec<u8>,
}

impl Duplicator {
    /// `gpu` - the frames stay on the GPU, for its encoder.
    pub fn new(display: &Display, gpu: bool) -> Result<Self> {
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                &display.adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let device = device.unwrap();
        let context = context.unwrap();
        // the encoder of the GPU uses the device from threads of its own
        if let Ok(guard) = device.cast::<ID3D11Multithread>() {
            unsafe { let _ = guard.SetMultithreadProtected(true); }
        }
        let duplication = unsafe { display.output.DuplicateOutput(&device)? };
        let desc = unsafe { duplication.GetDesc() };
        let (width, height) = (display.width, display.height); // the desktop coordinates: turned as the user sees them

        Ok(Self {
            display: display.clone(),
            device,
            context,
            duplication,
            gpu,
            held: None,
            staging: None,
            rotation: desc.Rotation,
            width,
            height,
            time: 0,
            bgra: Vec::new(),
            pointer: Pointer::default(),
            shape: Vec::new(),
        })
    }

    /// On the GPU: the texture of the last frame, as the panel shows it, while it is held.
    pub fn frame(&self) -> Option<&ID3D11Texture2D> { self.held.as_ref() }

    /// The frame held is let go: DXGI writes the desktop into it again.
    pub fn release(&mut self) {
        if self.held.take().is_some() {
            unsafe {
                let _ = self.duplication.ReleaseFrame();
            }
        }
    }

    /// The texture the frame is copied to for the CPU: made again when `texture` is of another size.
    fn staging(&mut self, texture: &ID3D11Texture2D) -> Result<()> {
        let mut d = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut d) };
        if matches!(&self.staging, Some((_, w, h)) if *w == d.Width && *h == d.Height) {
            return Ok(());
        }
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width:          d.Width,
            Height:         d.Height,
            MipLevels:      1,
            ArraySize:      1,
            Format:         d.Format,
            SampleDesc:     DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage:          D3D11_USAGE_STAGING,
            BindFlags:      0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags:      0,
        };
        let mut staging = None;
        unsafe { self.device.CreateTexture2D(&staging_desc, None, Some(&mut staging))? };
        self.staging = Some((staging.unwrap(), d.Width, d.Height));
        Ok(())
    }

    /// Waits for the next change, `timeout_ms` at most; the frame held is let go first. `Err` with `DXGI_ERROR_ACCESS_LOST` - the
    /// desktop switched ( a mode change, a secure desktop ): make a new one.
    pub fn next(&mut self, timeout_ms: u32) -> Result<Next> {
        self.release();
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe { self.duplication.AcquireNextFrame(timeout_ms, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(Next::Nothing),
            Err(e) => return Err(e),
        }
        let result = self.take(&info, resource);
        // a frame of the GPU is held for the encoder until the next acquisition, as DXGI advises; any other goes now
        if self.held.is_none() {
            unsafe {
                let _ = self.duplication.ReleaseFrame();
            }
        }
        result
    }

    fn take(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO, resource: Option<IDXGIResource>) -> Result<Next> {
        if info.LastMouseUpdateTime != 0 {
            self.pointer.visible = info.PointerPosition.Visible.as_bool();
            self.pointer.x = info.PointerPosition.Position.x;
            self.pointer.y = info.PointerPosition.Position.y;
        }
        if info.PointerShapeBufferSize != 0 {
            self.read_shape(info.PointerShapeBufferSize)?;
        }
        if info.LastPresentTime == 0 || info.AccumulatedFrames == 0 {
            return Ok(Next::Pointer);
        }
        let Some(resource) = resource else { return Ok(Next::Pointer) };
        let texture: ID3D11Texture2D = resource.cast()?;
        self.time = info.LastPresentTime;
        if self.gpu {
            self.held = Some(texture);
            return Ok(Next::Frame);
        }
        self.staging(&texture)?;
        // mapped when the frame goes to the encoder: a frame replaced before that is never read
        if let Some((staging, ..)) = &self.staging {
            unsafe { self.context.CopyResource(staging, &texture) };
        }
        Ok(Next::Frame)
    }

    /// The pixels of the last frame to `f`: BGRA rows `pitch` bytes apart, `width` × `height` at least. Read in place in the texture
    /// the CPU reads, unless the panel is turned: then the desktop is turned back into `bgra` first.
    pub fn pixels<R>(&mut self, f: impl FnOnce(&[u8], usize) -> R) -> Result<R> {
        let Some((staging, tw, th)) = &self.staging else { return Err(E_FAIL.into()) };
        let (tw, th) = (*tw as usize, *th as usize);
        let (dw, dh) = (self.width as usize, self.height as usize);
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let pitch = mapped.RowPitch as usize;
            let src = std::slice::from_raw_parts(mapped.pData as *const u8, pitch * th);
            let turned = match self.rotation {
                DXGI_MODE_ROTATION_ROTATE90 | DXGI_MODE_ROTATION_ROTATE270 => tw == dh && th == dw,
                DXGI_MODE_ROTATION_ROTATE180 => tw == dw && th == dh,
                _ => false,
            };
            if !turned && tw >= dw && th >= dh {
                let result = f(src, pitch);
                self.context.Unmap(staging, 0);
                return Ok(result);
            }
            self.bgra.resize(dw * dh * 4, 0);
            let dst = self.bgra.as_mut_slice();
            match self.rotation {
                // the panel shows the desktop turned: desktop ( x, y ) is the texture at the point it was turned to - the texture
                // turned clockwise by the rotation of the mode, as the video processor of src/win/gpu.rs turns it
                DXGI_MODE_ROTATION_ROTATE90 if turned => {
                    for y in 0..dh {
                        let d = &mut dst[y * dw * 4..][..dw * 4];
                        for x in 0..dw {
                            d[x * 4..x * 4 + 4].copy_from_slice(&src[(dw - 1 - x) * pitch + y * 4..][..4]);
                        }
                    }
                }
                DXGI_MODE_ROTATION_ROTATE270 if turned => {
                    for y in 0..dh {
                        let d = &mut dst[y * dw * 4..][..dw * 4];
                        let tx = (dh - 1 - y) * 4;
                        for x in 0..dw {
                            d[x * 4..x * 4 + 4].copy_from_slice(&src[x * pitch + tx..][..4]);
                        }
                    }
                }
                DXGI_MODE_ROTATION_ROTATE180 if turned => {
                    for y in 0..dh {
                        let d = &mut dst[y * dw * 4..][..dw * 4];
                        let s = &src[(dh - 1 - y) * pitch..][..dw * 4];
                        for x in 0..dw {
                            d[x * 4..x * 4 + 4].copy_from_slice(&s[(dw - 1 - x) * 4..][..4]);
                        }
                    }
                }
                _ => {
                    let row = dw.min(tw) * 4;
                    for y in 0..dh.min(th) {
                        dst[y * dw * 4..][..row].copy_from_slice(&src[y * pitch..][..row]);
                    }
                }
            }
            self.context.Unmap(staging, 0);
            Ok(f(&self.bgra, dw * 4))
        }
    }

    /// The new shape of the pointer, as BGRA with alpha: a monochrome shape and a masked one are made into one.
    fn read_shape(&mut self, size: u32) -> Result<()> {
        self.shape.resize(size as usize, 0);
        let mut needed = 0;
        let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        unsafe { self.duplication.GetFramePointerShape(size, self.shape.as_mut_ptr() as *mut _, &mut needed, &mut info)? };
        let (w, h, pitch) = (info.Width, info.Height, info.Pitch as usize);
        let kind = info.Type as i32;

        let (height, bgra) = if kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 {
            // AND mask over XOR mask, a bit a pixel: `h` is the height of both
            let h = h / 2;
            let mut out = vec![0u8; (w * h * 4) as usize];
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let bit = 0x80 >> (x % 8);
                    let and = self.shape[y * pitch + x / 8] & bit != 0;
                    let xor = self.shape[(y + h as usize) * pitch + x / 8] & bit != 0;
                    let p = &mut out[(y * w as usize + x) * 4..][..4];
                    match (and, xor) {
                        (false, false) => p.copy_from_slice(&[0, 0, 0, 255]),       // black
                        (false, true) => p.copy_from_slice(&[255, 255, 255, 255]),  // white
                        (true, false) => p.copy_from_slice(&[0, 0, 0, 0]),          // the screen shows through
                        (true, true) => p.copy_from_slice(&[0, 0, 0, 255]),         // inverts the screen: black stands for it
                    }
                }
            }
            (h, out)
        } else {
            let mut out = vec![0u8; (w * h * 4) as usize];
            for y in 0..h as usize {
                out[y * w as usize * 4..][..w as usize * 4].copy_from_slice(&self.shape[y * pitch..][..w as usize * 4]);
            }
            if kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 {
                // alpha 0xFF: XOR with the screen - drawn as a dark pixel; 0: the color itself
                for p in out.chunks_exact_mut(4) {
                    p[3] = if p[3] == 0 { 255 } else if p[0] | p[1] | p[2] == 0 { 0 } else { 255 };
                }
            } else if kind != DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 {
                return Ok(());
            }
            (h, out)
        };
        self.pointer.shape = Some((w, height, info.HotSpot.x as u32, info.HotSpot.y as u32, bgra));
        Ok(())
    }
}

/// The desktop switched or the mode changed: the duplication is to be made again.
pub fn lost(e: &windows::core::Error) -> bool { e.code() == DXGI_ERROR_ACCESS_LOST }
