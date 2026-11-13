// The machine on Android: the app of ../android loads this library ( librdp_host.so ) and starts the host of a computer in it. What
// only the app can do is the app's: the consent to show the screen ( MediaProjection ), the encoder of the phone that the shown
// screen goes into ( MediaCodec ), the sound it plays, its clipboard. The host asks for them by the static methods of
// org.unirail.RemoteDesk.Host and gets what they give by the native methods of the same class, below.
//
// Text crosses as UTF-8 bytes, both ways: the strings of JNI are "modified UTF-8", and an emoji of the host would not cross as one.

use std::ffi::{c_void, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Mutex, OnceLock};

use jni_sys::{jbyteArray, jclass, jint, jmethodID, jobject, jobjectArray, jvalue, JNIEnv, JavaVM, JNI_OK, JNI_VERSION_1_6};

pub mod clip;
pub mod files;
pub mod input;
pub mod marks;
pub mod mode;
pub mod rights;
pub mod sound;
pub mod video;

// ── the Java of the app ──

/// The methods of Host the host calls: looked up once, on the thread of the app that started it.
struct Java {
    vm:          *mut JavaVM,
    /// Host: a global reference - a thread of the host finds no class of the app by its name.
    host:        jclass,
    tell:        jmethodID,
    line:        jmethodID,
    video_start: jmethodID,
    video_stop:  jmethodID,
    key_frame:   jmethodID,
    bitrate:     jmethodID,
    set_clip:    jmethodID,
    sound_start: jmethodID,
    sound_stop:  jmethodID,
    mark_new:    jmethodID,
    mark_show:   jmethodID,
    mark_fade:   jmethodID,
    mark_gone:   jmethodID,
}

// SAFETY: the VM, a global reference and the ids of methods are for every thread
unsafe impl Send for Java {}
unsafe impl Sync for Java {}

static JAVA: OnceLock<Java> = OnceLock::new();

/// A thread of the host attached to the VM while it lives: detached when it ends, as Android wants.
struct Attached {
    env:  *mut JNIEnv,
    ours: bool,
}

impl Drop for Attached {
    fn drop(&mut self) {
        if self.ours {
            if let Some(java) = JAVA.get() {
                unsafe { ((**java.vm).v1_4.DetachCurrentThread)(java.vm) };
            }
        }
    }
}

thread_local! {
    static ATTACHED: Attached = attach();
}

fn attach() -> Attached {
    let Some(java) = JAVA.get() else { return Attached { env: null_mut(), ours: false } };
    let mut env: *mut c_void = null_mut();
    unsafe {
        if ((**java.vm).v1_4.GetEnv)(java.vm, &mut env, JNI_VERSION_1_6) == JNI_OK {
            return Attached { env: env as *mut JNIEnv, ours: false }; // a thread of the app
        }
        if ((**java.vm).v1_4.AttachCurrentThread)(java.vm, &mut env, null_mut()) == JNI_OK {
            return Attached { env: env as *mut JNIEnv, ours: true };
        }
    }
    Attached { env: null_mut(), ours: false }
}

/// Calls `f` with the environment of this thread: nothing when the app has not started the host.
fn with<T>(f: impl FnOnce(*mut JNIEnv, &Java) -> T) -> Option<T> {
    let java = JAVA.get()?;
    let env = ATTACHED.with(|a| a.env);
    if env.is_null() {
        return None;
    }
    let result = f(env, java);
    unsafe {
        // what the app threw is its own: written to its log, the host goes on
        if ((**env).v1_6.ExceptionCheck)(env) {
            ((**env).v1_6.ExceptionDescribe)(env);
            ((**env).v1_6.ExceptionClear)(env);
        }
    }
    Some(result)
}

/// `data` as a byte[] of Java: a local reference, let go by the caller.
unsafe fn bytes(env: *mut JNIEnv, data: &[u8]) -> jbyteArray {
    let array = ((**env).v1_6.NewByteArray)(env, data.len() as jint);
    if !array.is_null() {
        ((**env).v1_6.SetByteArrayRegion)(env, array, 0, data.len() as jint, data.as_ptr() as *const i8);
    }
    array
}

/// A byte[] of Java as a String of UTF-8.
unsafe fn text(env: *mut JNIEnv, array: jbyteArray) -> String {
    if array.is_null() {
        return String::new();
    }
    let n = ((**env).v1_6.GetArrayLength)(env, array).max(0) as usize;
    let mut data = vec![0u8; n];
    ((**env).v1_6.GetByteArrayRegion)(env, array, 0, n as jint, data.as_mut_ptr() as *mut i8);
    String::from_utf8(data).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// A static void method of Host with these arguments.
fn void(method: impl Fn(&Java) -> jmethodID, args: &[jvalue]) {
    with(|env, java| unsafe { ((**env).v1_6.CallStaticVoidMethodA)(env, java.host, method(java), args.as_ptr()) });
}

/// A static method of Host with text arguments, the byte[] let go after.
fn void_texts(method: impl Fn(&Java) -> jmethodID, first: &str, tone: Option<jint>, second: Option<&str>) {
    with(|env, java| unsafe {
        let a = bytes(env, first.as_bytes());
        let b = second.map(|s| bytes(env, s.as_bytes())).unwrap_or(null_mut());
        let mut args = vec![jvalue { l: a }];
        if let Some(t) = tone {
            args.push(jvalue { i: t });
        }
        if second.is_some() {
            args.push(jvalue { l: b });
        }
        ((**env).v1_6.CallStaticVoidMethodA)(env, java.host, method(java), args.as_ptr());
        ((**env).v1_6.DeleteLocalRef)(env, a);
        if !b.is_null() {
            ((**env).v1_6.DeleteLocalRef)(env, b);
        }
    });
}

// ── what the console of a computer does, on the screen of the app ──

/// One piece of news on the screen of the app: `tone` is the index of `console::Tone`.
pub fn tell(sign: &str, tone: jint, text: &str) { void_texts(|j| j.tell, sign, Some(tone), Some(text)); }

/// The line for whoever connects: the app shows it, puts it in the clipboard of the phone and offers to share it.
pub fn line(line: &str) { void_texts(|j| j.line, line, None, None); }

/// What the user typed or pressed in the app where a console would be read: one line, None never - the app lives as long as the host.
pub fn answer() -> Option<String> { answers().1.lock().unwrap().recv().ok() }

fn answers() -> &'static (Mutex<Sender<String>>, Mutex<Receiver<String>>) {
    static ANSWERS: OnceLock<(Mutex<Sender<String>>, Mutex<Receiver<String>>)> = OnceLock::new();
    ANSWERS.get_or_init(|| {
        let (sender, receiver) = channel();
        (Mutex::new(sender), Mutex::new(receiver))
    })
}

// ── the start: the program of a computer starts by main, the app by Host.start ──

/// Nothing to ask: the app asked for its permissions before it started the host.
pub fn init() {}

/// The host of a computer runs `run` on its main thread; on the phone the app starts it ( `Host.start` ).
#[allow(dead_code)]
pub fn main(run: fn()) { run(); }

static STARTED: AtomicBool = AtomicBool::new(false);

/// The folder the files from the viewer go to.
static DOWNLOADS: OnceLock<PathBuf> = OnceLock::new();

/// Runs `f`, a native method of Host: a panic stays here, it never unwinds into the VM.
fn guarded(what: &str, f: impl FnOnce()) {
    if catch_unwind(AssertUnwindSafe(f)).is_err() {
        tell("💥", 4, &format!("{what} failed"));
    }
}

unsafe fn method(env: *mut JNIEnv, class: jclass, name: &str, signature: &str) -> jmethodID {
    let n = CString::new(name).unwrap();
    let s = CString::new(signature).unwrap();
    ((**env).v1_6.GetStaticMethodID)(env, class, n.as_ptr(), s.as_ptr())
}

/// `Host.start( way, arguments, name, crypto, downloads )`: the way chosen in the app ( 0 - this network … 3 - directly ), the arguments of
/// a computer that it makes, the name of the phone, the path of the library of encryption, the folder of the files that come.
#[no_mangle]
pub unsafe extern "system" fn Java_org_unirail_RemoteDesk_Host_start(env: *mut JNIEnv, class: jclass, way: jint, arguments: jobjectArray, name: jbyteArray, crypto: jbyteArray, downloads: jbyteArray) {
    if STARTED.swap(true, Ordering::AcqRel) {
        return; // one host a process
    }
    let mut vm: *mut JavaVM = null_mut();
    if ((**env).v1_6.GetJavaVM)(env, &mut vm) != JNI_OK {
        return;
    }
    let host = ((**env).v1_6.NewGlobalRef)(env, class);
    let _ = JAVA.set(Java {
        vm,
        host,
        tell: method(env, class, "tell", "([BI[B)V"),
        line: method(env, class, "line", "([B)V"),
        video_start: method(env, class, "videoStart", "(II)J"),
        video_stop: method(env, class, "videoStop", "()V"),
        key_frame: method(env, class, "keyFrame", "()V"),
        bitrate: method(env, class, "bitrate", "(I)V"),
        set_clip: method(env, class, "setClip", "([B)Z"),
        sound_start: method(env, class, "soundStart", "()Z"),
        sound_stop: method(env, class, "soundStop", "()V"),
        mark_new: method(env, class, "markNew", "()I"),
        mark_show: method(env, class, "markShow", "(IIIIILjava/nio/ByteBuffer;F)V"),
        mark_fade: method(env, class, "markFade", "(IF)V"),
        mark_gone: method(env, class, "markGone", "(I)V"),
    });
    let mut args = Vec::new();
    let n = ((**env).v1_6.GetArrayLength)(env, arguments);
    for i in 0..n {
        let a = ((**env).v1_6.GetObjectArrayElement)(env, arguments, i);
        args.push(text(env, a));
        ((**env).v1_6.DeleteLocalRef)(env, a);
    }
    // the name the viewers see, and where the library of encryption lies: as a computer has them in its environment
    std::env::set_var("COMPUTERNAME", text(env, name));
    std::env::set_var("ADHOC_CRYPTO_NATIVE", text(env, crypto));
    let _ = DOWNLOADS.set(PathBuf::from(text(env, downloads)));
    let way = match way {
        0 => crate::menu::Way::Here,
        1 => crate::menu::Way::Internet,
        2 => crate::menu::Way::Meet,
        _ => crate::menu::Way::Direct,
    };
    crate::menu::given(way, args);
    let _ = std::thread::Builder::new().name("host".into()).spawn(|| {
        if catch_unwind(crate::session::run).is_err() {
            tell("💥", 4, "The host stopped: start it again");
        }
    });
}

/// `Host.screen( width, height )`: the size of the screen of the phone, in pixels - at the start, and when it turns.
#[no_mangle]
pub unsafe extern "system" fn Java_org_unirail_RemoteDesk_Host_screen(_env: *mut JNIEnv, _class: jclass, width: jint, height: jint) {
    guarded("the size of the screen", || video::screen_is(width.max(0) as u32, height.max(0) as u32));
}

/// `Host.unit( buffer, offset, size, flags )`: what the encoder gave - a direct ByteBuffer of MediaCodec, its flags.
#[no_mangle]
pub unsafe extern "system" fn Java_org_unirail_RemoteDesk_Host_unit(env: *mut JNIEnv, _class: jclass, buffer: jobject, offset: jint, size: jint, flags: jint) {
    let at = ((**env).v1_6.GetDirectBufferAddress)(env, buffer) as *const u8;
    if at.is_null() || offset < 0 || size <= 0 {
        return;
    }
    let data = std::slice::from_raw_parts(at.add(offset as usize), size as usize);
    guarded("a frame", || video::unit(data, flags));
}

/// `Host.ended()`: the phone stopped showing the screen - its user did, or the system.
#[no_mangle]
pub unsafe extern "system" fn Java_org_unirail_RemoteDesk_Host_ended(_env: *mut JNIEnv, _class: jclass) {
    guarded("the end of the screen", video::ended);
}

/// `Host.sound( buffer, size )`: what the phone played - a direct ByteBuffer, 48 kHz stereo, 16-bit little endian.
#[no_mangle]
pub unsafe extern "system" fn Java_org_unirail_RemoteDesk_Host_sound(env: *mut JNIEnv, _class: jclass, buffer: jobject, size: jint) {
    let at = ((**env).v1_6.GetDirectBufferAddress)(env, buffer) as *const u8;
    if at.is_null() || size <= 0 {
        return;
    }
    let data = std::slice::from_raw_parts(at, size as usize);
    guarded("the sound", || sound::deliver(data));
}

/// `Host.clip( text )`: text the user of the phone sent to the computer - selected and sent, shared, or the clipboard sent.
#[no_mangle]
pub unsafe extern "system" fn Java_org_unirail_RemoteDesk_Host_clip(env: *mut JNIEnv, _class: jclass, data: jbyteArray) {
    let t = text(env, data);
    guarded("the text sent", || clip::sent(t));
}

/// `Host.answer( text )`: what the user typed or pressed where a computer would read its console.
#[no_mangle]
pub unsafe extern "system" fn Java_org_unirail_RemoteDesk_Host_answer(env: *mut JNIEnv, _class: jclass, data: jbyteArray) {
    let t = text(env, data);
    let _ = answers().0.lock().unwrap().send(t);
}

// ── the calls of the modules ──

/// `Host.videoStart( fps, bitrate )`: the size of the frames the encoder of the phone makes; None - it did not start.
fn video_start(fps: u32, bitrate: u32) -> Option<(u32, u32)> {
    let packed = with(|env, java| unsafe {
        let args = [jvalue { i: fps as jint }, jvalue { i: bitrate.min(i32::MAX as u32) as jint }];
        ((**env).v1_6.CallStaticLongMethodA)(env, java.host, java.video_start, args.as_ptr())
    })?;
    let packed = packed as u64; // width << 32 | height
    (packed != 0).then_some(((packed >> 32) as u32, packed as u32))
}

fn video_stop() { void(|j| j.video_stop, &[]); }
fn key_frame() { void(|j| j.key_frame, &[]); }
fn bitrate(bitrate: u32) { void(|j| j.bitrate, &[jvalue { i: bitrate.min(i32::MAX as u32) as jint }]); }

fn set_clip(text: &str) -> bool {
    with(|env, java| unsafe {
        let a = bytes(env, text.as_bytes());
        let args = [jvalue { l: a }];
        let done = ((**env).v1_6.CallStaticBooleanMethodA)(env, java.host, java.set_clip, args.as_ptr());
        ((**env).v1_6.DeleteLocalRef)(env, a);
        done
    })
    .unwrap_or(false)
}

fn sound_start() -> bool {
    with(|env, java| unsafe { ((**env).v1_6.CallStaticBooleanMethodA)(env, java.host, java.sound_start, [].as_ptr()) }).unwrap_or(false)
}

fn sound_stop() { void(|j| j.sound_stop, &[]); }

/// `Host.markNew()`: a new image in the window of the app over the others; None, or below 0 - the app may not draw over them.
fn mark_new() -> Option<jint> { with(|env, java| unsafe { ((**env).v1_6.CallStaticIntMethodA)(env, java.host, java.mark_new, [].as_ptr()) }) }

/// `Host.markShow( id, x, y, width, height, pixels, alpha )`: the pixels in a direct buffer over this memory, copied by the app at once.
fn mark_show(id: jint, x: i32, y: i32, width: u32, height: u32, pixels: &[u8], alpha: f32) {
    with(|env, java| unsafe {
        let buffer = ((**env).v1_6.NewDirectByteBuffer)(env, pixels.as_ptr() as *mut c_void, pixels.len() as i64);
        if buffer.is_null() {
            return;
        }
        let args = [jvalue { i: id }, jvalue { i: x }, jvalue { i: y }, jvalue { i: width as jint }, jvalue { i: height as jint }, jvalue { l: buffer }, jvalue { f: alpha }];
        ((**env).v1_6.CallStaticVoidMethodA)(env, java.host, java.mark_show, args.as_ptr());
        ((**env).v1_6.DeleteLocalRef)(env, buffer);
    });
}

fn mark_fade(id: jint, alpha: f32) { void(|j| j.mark_fade, &[jvalue { i: id }, jvalue { f: alpha }]); }

fn mark_gone(id: jint) { void(|j| j.mark_gone, &[jvalue { i: id }]); }
