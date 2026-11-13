// The files on Android: those from the viewer go to the folder of the app the app named at the start ( Android/data/…/files/Download,
// seen in the app Files ). A file to the viewer its user picks in the picker of Android, shown by the app ( Host.pickFile ): the app
// gives a path that opens the file it picked, under its name - nothing is copied.

use std::path::PathBuf;

/// The folder of the files from the viewer.
pub fn downloads() -> std::io::Result<PathBuf> {
    let dir = super::DOWNLOADS.get().cloned().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no folder for the files"))?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// The picker of Android on the screen of the phone: the file its user picked, none when cancelled.
pub fn pick() -> Option<PathBuf> { super::pick_file().filter(|p| !p.is_empty()).map(PathBuf::from) }
