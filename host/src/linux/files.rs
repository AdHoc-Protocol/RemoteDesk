// The files on Linux: the Downloads folder of the user ( XDG ), the dialog to pick a file - of the portal FileChooser.

use std::path::PathBuf;

/// The Downloads folder of the user.
pub fn downloads() -> std::io::Result<PathBuf> {
    dirs::download_dir().or_else(|| dirs::home_dir().map(|h| h.join("Downloads"))).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no Downloads folder"))
}

/// The dialog of the desktop on the screen of the machine: the file its user picked, none when cancelled.
pub fn pick() -> Option<PathBuf> { rfd::FileDialog::new().set_title("RemoteDesk: the file to send to the viewer").pick_file() }
