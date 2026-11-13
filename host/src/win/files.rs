// The files on Windows: the Downloads folder of the user, the dialog to pick a file.

use std::path::PathBuf;

use windows::core::w;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::{FileOpenDialog, FOLDERID_Downloads, IFileOpenDialog, SHGetKnownFolderPath, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, KNOWN_FOLDER_FLAG, SIGDN_FILESYSPATH};

/// The Downloads folder of the user.
pub fn downloads() -> std::io::Result<PathBuf> {
    unsafe {
        let path = SHGetKnownFolderPath(&FOLDERID_Downloads, KNOWN_FOLDER_FLAG(0), None).map_err(|e| std::io::Error::from_raw_os_error(e.code().0 & 0xFFFF))?;
        let folder = PathBuf::from(path.to_string().unwrap_or_default());
        CoTaskMemFree(Some(path.0 as _));
        Ok(folder)
    }
}

/// The dialog of Windows on the screen of the machine: the file its user picked, none when cancelled.
pub fn pick() -> Option<PathBuf> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let _ = dialog.SetTitle(w!("RemoteDesk: the file to send to the viewer"));
        if let Ok(options) = dialog.GetOptions() {
            let _ = dialog.SetOptions(options | FOS_FILEMUSTEXIST | FOS_FORCEFILESYSTEM);
        }
        dialog.Show(None).ok()?; // cancelled
        let item = dialog.GetResult().ok()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = PathBuf::from(name.to_string().ok()?);
        CoTaskMemFree(Some(name.0 as _));
        Some(path)
    }
}
