// The files on Android: those from the viewer go to the folder of the app the app named at the start ( Android/data/…/files/Download,
// seen in the app Files ).
//
// TODO(port): `pick` - the picker of Android ( ACTION_OPEN_DOCUMENT ), shown by the app on the screen of the phone.

use std::path::PathBuf;

/// The folder of the files from the viewer.
pub fn downloads() -> std::io::Result<PathBuf> {
    let dir = super::DOWNLOADS.get().cloned().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no folder for the files"))?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// No file is picked on the phone yet.
pub fn pick() -> Option<PathBuf> {
    say!("📁", Warn, "This phone does not send files yet: it takes the ones the viewer sends");
    None
}
