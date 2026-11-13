// Files both ways, one at a time each way for each viewer: `Metadata`, `Data` in chunks, `Done`; the receiver answers `Success` or
// `Error`.
//
// To the machine - only from the viewer that controls it: the file of the page is written to the Downloads folder of the user, under a
// name no file there has yet.
// From the machine - to any viewer: the page asks, the user of the machine picks the file in the dialog of the system, it goes out in
// chunks of 64 KiB - no more than `AHEAD` of them wait to be sent, so the frames of the video, in the same queue, are not held behind a
// file; each chunk sent wakes the sender for the next.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::gen::__::*;
use crate::gen::session::{actor0, Actor0, Connection};

const CHUNK: usize = 64 * 1024;
/// Chunks of a file queued and not yet sent: 128 KiB at most in front of a frame of the video.
const AHEAD: u64 = 2;

/// The file coming from a page.
enum Incoming {
    None,
    Writing { file: File, path: PathBuf, written: u64 },
    /// The transfer failed or was cancelled: what is still on the wire is dropped until the next `Metadata`.
    Dropping,
}

/// The files coming from the pages, by the address of the connection.
static INCOMING: Mutex<Vec<(usize, Incoming)>> = Mutex::new(Vec::new());

/// A file going to a page.
struct Outgoing {
    conn:   usize,
    /// Set to stop it.
    cancel: AtomicBool,
    /// Chunks of it the transport has sent.
    sent:   AtomicU64,
    /// The thread sending it: a chunk sent wakes it.
    sender: OnceLock<std::thread::Thread>,
}

/// The files going to the pages.
static OUTGOING: Mutex<Vec<Arc<Outgoing>>> = Mutex::new(Vec::new());

fn key(conn: &Connection) -> usize { conn as *const Connection as usize }

/// The transfer from the page on `conn`, with `incoming` locked.
fn incoming_of<'a>(incoming: &'a mut Vec<(usize, Incoming)>, conn: &Connection) -> &'a mut Incoming {
    let key = key(conn);
    let at = match incoming.iter().position(|(c, _)| *c == key) {
        Some(at) => at,
        None => {
            incoming.push((key, Incoming::None));
            incoming.len() - 1
        }
    };
    &mut incoming[at].1
}

/// The file going to the page on `conn` stops.
fn cancel_outgoing(conn: &Connection) -> bool {
    let key = key(conn);
    let mut outgoing = OUTGOING.lock().unwrap();
    let Some(at) = outgoing.iter().position(|o| o.conn == key) else { return false };
    let o = outgoing.swap_remove(at);
    o.cancel.store(true, Ordering::Release);
    if let Some(sender) = o.sender.get() {
        sender.unpark();
    }
    true
}

pub fn register() {
    actor0::working::on_receive_d::FileTransfer_Metadata.add(on_metadata);
    actor0::working::on_receive_d::FileTransfer_Data.add(on_data);
    actor0::working::on_receive_d::FileTransfer_Done.add(on_done);
    actor0::working::on_receive_d::FileTransfer_Error.add(on_error);
    actor0::working::on_receive_d::FileTransfer_Success.add(on_success);
    actor0::working::on_receive_d::FileTransfer_RequestTransfer.add(on_request);
    actor0::working::on_serialize_d::FileTransfer_Data.add(on_chunk_sent);
}

/// The viewer on `conn` left: a file of it half written is removed, a file to it half sent stops.
pub fn stop(conn: &Connection) {
    let mut incoming = INCOMING.lock().unwrap();
    if let Some(at) = incoming.iter().position(|(c, _)| *c == key(conn)) {
        abandon(&mut incoming.swap_remove(at).1, Incoming::None);
    }
    drop(incoming);
    cancel_outgoing(conn);
}

// ── to the machine ──

fn on_metadata(meta: &mut FileTransfer_Metadata, conn: &Connection, _actor: &Actor0) {
    let mut all = INCOMING.lock().unwrap();
    let incoming = incoming_of(&mut all, conn);
    abandon(incoming, Incoming::None);
    if !crate::session::controls(conn) {
        *incoming = Incoming::Dropping;
        return send_kind(conn, FileTransfer_Error_Kind::PERMISSION_DENIED);
    }
    let name = safe_name(meta.filename.as_deref().unwrap_or_default());
    let opened = crate::os::files::downloads().and_then(|folder| create_new(&folder, &name));
    match opened {
        Ok((file, path)) => {
            say!("📥", News, "Receiving {} ( {} bytes )", path.display(), meta.size);
            *incoming = Incoming::Writing { file, path, written: 0 };
        }
        Err(e) => {
            say!("📥", Bad, "Cannot create {name}: {e}");
            *incoming = Incoming::Dropping;
            send_error(conn, &e);
        }
    }
}

fn on_data(data: &mut FileTransfer_Data, conn: &Connection, _actor: &Actor0) {
    let mut all = INCOMING.lock().unwrap();
    let incoming = incoming_of(&mut all, conn);
    let failed = match incoming {
        Incoming::Writing { file, written, .. } => {
            let bytes = data.data.as_deref().unwrap_or_default();
            match file.write_all(bytes) {
                Ok(()) => {
                    *written += bytes.len() as u64;
                    None
                }
                Err(e) => Some(e),
            }
        }
        Incoming::None => {
            *incoming = Incoming::Dropping;
            return send_kind(conn, FileTransfer_Error_Kind::PROTOCOL_ERROR);
        }
        Incoming::Dropping => return,
    };
    if let Some(e) = failed {
        say!("📥", Bad, "Cannot write the file: {e}");
        abandon(incoming, Incoming::Dropping);
        send_error(conn, &e);
    }
}

fn on_done(_done: &mut FileTransfer_Done, conn: &Connection, _actor: &Actor0) {
    let mut all = INCOMING.lock().unwrap();
    match std::mem::replace(incoming_of(&mut all, conn), Incoming::None) {
        // the file goes to the disk on a thread of its own: the receiving of the connection does not wait for the disk
        Incoming::Writing { file, path, written } => {
            let Some(conn) = crate::session::viewer(conn) else { return };
            std::thread::spawn(move || match file.sync_all() {
                Ok(()) => {
                    say!("📥", Good, "Received {} ( {written} bytes )", path.display());
                    actor0::working::transmitter::send_file_transfer_success(FileTransfer_Success, &conn);
                }
                Err(e) => {
                    drop(file);
                    let _ = std::fs::remove_file(&path);
                    send_error(&conn, &e);
                }
            });
        }
        Incoming::None => send_kind(conn, FileTransfer_Error_Kind::PROTOCOL_ERROR),
        Incoming::Dropping => {}
    }
}

/// The page stopped the transfer, or failed to save the file of the machine.
fn on_error(error: FileTransfer_Error, conn: &Connection, _actor: &Actor0) {
    let mut all = INCOMING.lock().unwrap();
    let incoming = incoming_of(&mut all, conn);
    if matches!(*incoming, Incoming::Writing { .. }) {
        say!("📁", Quiet, "The page stopped the transfer ( {:?} )", error.kind().0);
        abandon(incoming, Incoming::Dropping);
    }
    drop(all);
    if cancel_outgoing(conn) {
        say!("📁", Warn, "The page did not take the file ( {} )", error.kind().0);
    }
}

/// The file half written is removed.
fn abandon(incoming: &mut Incoming, next: Incoming) {
    if let Incoming::Writing { file, path, .. } = std::mem::replace(incoming, next) {
        drop(file);
        let _ = std::fs::remove_file(&path);
    }
}

/// `name`, or `name (2)`, `name (3)`… - the first no file in `folder` has.
fn create_new(folder: &Path, name: &str) -> std::io::Result<(File, PathBuf)> {
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    for n in 1..1000 {
        let path = if n == 1 { folder.join(name) } else { folder.join(format!("{stem} ({n}){ext}")) };
        match File::options().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists))
}

/// The last part of what the page named, with what Windows does not take in a name replaced.
fn safe_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut safe: String = last.chars().map(|c| if c < ' ' || "<>:\"|?*".contains(c) { '_' } else { c }).collect();
    while safe.ends_with(['.', ' ']) {
        safe.pop();
    }
    if safe.is_empty() {
        return String::from("file");
    }
    let base = safe.split('.').next().unwrap_or_default().trim_end().to_ascii_uppercase();
    let reserved = matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL") || (base.len() == 4 && (base.starts_with("COM") || base.starts_with("LPT")) && base.as_bytes()[3].is_ascii_digit());
    if reserved {
        safe.insert(0, '_');
    }
    safe
}

fn send_error(conn: &Connection, e: &std::io::Error) {
    let kind = match e.kind() {
        std::io::ErrorKind::StorageFull => FileTransfer_Error_Kind::OUT_OF_DISK_SPACE,
        std::io::ErrorKind::PermissionDenied => FileTransfer_Error_Kind::PERMISSION_DENIED,
        _ => FileTransfer_Error_Kind::IO_ERROR,
    };
    let mut error = FileTransfer_Error::default();
    error.set_kind(kind);
    if let Some(code) = e.raw_os_error() {
        error.set_api_error_code(code);
    }
    actor0::working::transmitter::send_file_transfer_error(error, conn);
}

fn send_kind(conn: &Connection, kind: FileTransfer_Error_Kind) {
    let mut error = FileTransfer_Error::default();
    error.set_kind(kind);
    actor0::working::transmitter::send_file_transfer_error(error, conn);
}

// ── from the machine ──

fn on_success(_success: &mut FileTransfer_Success, _conn: &Connection, _actor: &Actor0) {
    say!("📤", Good, "The page saved the file");
}

fn on_chunk_sent(_data: &mut FileTransfer_Data, conn: &Connection, _actor: &Actor0) {
    let key = key(conn);
    if let Some(o) = OUTGOING.lock().unwrap().iter().find(|o| o.conn == key) {
        o.sent.fetch_add(1, Ordering::AcqRel);
        if let Some(sender) = o.sender.get() {
            sender.unpark();
        }
    }
}

/// A page asks for a file: the user of the machine picks it - on a thread of its own, the dialog waits for the user.
fn on_request(_request: &mut FileTransfer_RequestTransfer, conn: &Connection, _actor: &Actor0) {
    let Some(conn) = crate::session::viewer(conn) else { return };
    let transfer = {
        let mut outgoing = OUTGOING.lock().unwrap();
        let key = key(&conn);
        if outgoing.iter().any(|o| o.conn == key) {
            return say!("📤", Warn, "A file is on its way to the page already");
        }
        let transfer = Arc::new(Outgoing { conn: key, cancel: AtomicBool::new(false), sent: AtomicU64::new(0), sender: OnceLock::new() });
        outgoing.push(transfer.clone());
        transfer
    };
    std::thread::Builder::new()
        .name("file to the page".into())
        .spawn(move || {
            let _ = transfer.sender.set(std::thread::current());
            match crate::os::files::pick() {
                Some(path) => send_file(&conn, &path, &transfer),
                None => {
                    say!("📤", Quiet, "No file picked");
                    send_kind(&conn, FileTransfer_Error_Kind::CANCELED);
                }
            }
            OUTGOING.lock().unwrap().retain(|o| !Arc::ptr_eq(o, &transfer));
        })
        .expect("the thread of a file");
}

fn send_file(conn: &Arc<Connection>, path: &Path, transfer: &Outgoing) {
    let cancel = &transfer.cancel;
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            say!("📤", Bad, "Cannot open {}: {e}", path.display());
            return send_error(conn, &e);
        }
    };
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let filename = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    say!("📤", News, "Sending {} ( {size} bytes )", path.display());

    let open = || conn.ext_connection().is_some();
    if cancel.load(Ordering::Acquire) || !open() {
        return say!("📤", Quiet, "The page no longer waits for {}", path.display());
    }
    if !actor0::working::transmitter::send_file_transfer_metadata(FileTransfer_Metadata { filename: Some(filename), size }, conn) {
        return say!("📤", Bad, "The connection takes no more");
    }

    let mut queued = 0u64;
    let mut offset = 0u64;
    loop {
        if cancel.load(Ordering::Acquire) || !open() {
            return say!("📤", Quiet, "Stopped at {offset} bytes");
        }
        if queued - transfer.sent.load(Ordering::Acquire) >= AHEAD {
            std::thread::park_timeout(Duration::from_millis(100)); // a chunk sent wakes it, and the cancel; the time - for a link that closed
            continue;
        }
        let mut chunk = vec![0u8; CHUNK];
        let mut filled = 0;
        while filled < CHUNK {
            match read_at(&file, &mut chunk[filled..], offset + filled as u64) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    say!("📤", Bad, "Cannot read {}: {e}", path.display());
                    return send_error(conn, &e);
                }
            }
        }
        if filled == 0 {
            break;
        }
        chunk.truncate(filled);
        // the queue is full for a moment: the chunk is read again - the pack refused is gone
        if actor0::working::transmitter::send_file_transfer_data(FileTransfer_Data { data: Some(chunk) }, conn) {
            queued += 1;
            offset += filled as u64;
        } else {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    while !actor0::working::transmitter::send_file_transfer_done(FileTransfer_Done, conn) {
        if cancel.load(Ordering::Acquire) || !open() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    say!("📤", Good, "Sent {} ( {offset} bytes )", path.display());
}

/// Bytes of `file` from `offset` on, wherever its cursor is.
fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    #[cfg(windows)]
    return std::os::windows::fs::FileExt::seek_read(file, buffer, offset);
    #[cfg(unix)]
    return std::os::unix::fs::FileExt::read_at(file, buffer, offset);
}
