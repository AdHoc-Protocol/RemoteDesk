// The messages between the page ( desk.ts, the main thread ) and the worker of the session ( worker/session.ts ): small objects
// told apart by `t`, numbers and strings in them, bytes moved - transferred - not copied. The page posts the input once an
// animation frame at most, the worker the numbers of the video once a second. The worker sends no words: what happened, by codes - the
// page says it in the language of its texts ( `Texts` of desk.ts ).

/** What the page tells the worker. */
export const enum W {
    /** Dial the machine: the canvas the video is drawn on goes with it, one for each connection. */
    CONNECT,
    /** The user leaves: the connection closes. */
    LEAVE,
    /** What the pointer and the wheel did in one animation frame of the page. */
    POINTER,
    /** A burst of input begins: the machine returns the time with the first frame after it. */
    STAMP,
    KEY,
    TEXT,
    BUTTON,
    TOUCH,
    /** The page is seen, or not: a page not seen takes no video. */
    VISIBLE,
    FPS,
    /** The resolution of the machine follows the size of the screen of the page, or is restored. */
    FIT,
    /** The display to show. */
    SELECT,
    /** Ask for the rights of an administrator on the machine. */
    ELEVATE,
    /** The sound of the machine on or off. */
    SOUND,
    /** The port of the worklet that plays the sound of the machine. */
    PLAYER,
    /** The port of the worklet of the microphone: it is on. */
    MIC,
    /** The microphone is off; `tell` - the machine is told so. */
    MIC_OFF,
    /** The text on the clipboard of this device: sent to the machine when it is new. */
    CLIPBOARD,
    /** Files to the machine. */
    FILES,
    /** A file from the machine: its user picks it. */
    FILE_REQUEST,
    FILE_CANCEL,
    /** This viewer takes the control of the machine. */
    TAKE_CONTROL,
}

/** What the worker tells the page. */
export const enum M {
    /** The key the machine proved by the handshake, and whether it is the key kept for the address. */
    PROVED,
    /** The connection failed: why, in the words of the browser. */
    STATUS,
    /** The connect is done: `ok` - the socket is up, the login is sent. */
    DIALED,
    WELCOME,
    DENIED,
    /** Who watches the machine and who controls it: the input, the fit, the microphone, the clipboard and the files to the machine are
     *  of the one that controls. */
    AUDIENCE,
    /** The connection of the session closed. */
    CLOSED,
    /** How the request for the rights of an administrator goes: a notice of the bar. */
    NOTICE,
    /** The rights of an administrator on the machine. */
    ADMIN,
    /** The size of the frames: the canvas has it now. */
    SIZE,
    /** The display shown now. */
    SCREEN,
    DISPLAYS,
    /** A new shape of the pointer of the machine. */
    CURSOR,
    /** Where the pointer of the machine is. */
    HOST_AT,
    /** How the frames went in the last second. */
    STATS,
    /** Text the machine copied. */
    CLIPBOARD,
    /** How the transfer of a file goes. */
    FILE_STATUS,
    /** A file from the machine, all of it, to save. */
    DOWNLOAD,
    /** The machine cannot play the microphone. */
    MIC_ERROR,
}

/** The kinds of a touch event. */
export const enum Touch { START, MOVE, END, CANCEL }

/** The numbers of a touch point in `TOUCH.points`, one point after another: id, x, y, radius x, radius y, angle, pressure. */
export const TOUCH_POINT = 7;

/** The rights of an administrator: not yet - the button asks for them; there; not needed by the system of the machine. */
export const enum Admin { ASK, YES, NOT_NEEDED }

/** Why the machine refused the login. */
export const enum Refusal { UNEXPECTED, WRONG_TOKEN, OVERLOAD, OTHER, FULL }

/** The notice of the request for the rights of an administrator: none; the user of the machine is asked; confirmed; declined; failed. */
export const enum Elevation { NONE, ASKING, CONFIRMED, DECLINED, FAILED }

/** The line of a transfer of a file: how far; saved on the machine; received here; failed; wait - one goes on; the user picks; cancelled. */
export const enum FileLine { PROGRESS, SAVED, RECEIVED, FAILED, WAIT, PICKING, CANCELLED }

/** Why a transfer failed. */
export const enum FileWhy { CANCELLED, PROTOCOL, PERMISSION, DISK, IO, OTHER, CLOSED, ENDED }

export interface Display {
    screen: number;
    name: string;
    width: number;
    height: number;
}

/** x and y are fractions of the display, 0 … 65535. */
export type ToWorker =
    | { t: W.CONNECT; address: string; token: string; name: string; secret: Uint8Array; kept: Uint8Array | undefined; origin: number; canvas: OffscreenCanvas; port?: MessagePort | undefined }
    | { t: W.LEAVE }
    /** `moved` - the pointer is at x, y; dx, dy - pixels of the machine the locked pointer moved; wx, wy - 120ths of a notch. */
    | { t: W.POINTER; moved: boolean; x: number; y: number; dx: number; dy: number; wx: number; wy: number }
    /** `ms` - `performance.now()` of the page. */
    | { t: W.STAMP; ms: number }
    /** `usb` - the USB HID usage; the locks are told only with a key going down. */
    | { t: W.KEY; usb: number; pressed: boolean; caps?: boolean; num?: boolean }
    | { t: W.TEXT; text: string }
    /** `button` - of `PointerEvent.button`, 0 … 4; `locked` - the pointer is locked in the page: the click goes where the pointer of the machine is. */
    | { t: W.BUTTON; button: number; down: boolean; x: number; y: number; locked: boolean }
    /** `points` - `TOUCH_POINT` numbers each. */
    | { t: W.TOUCH; kind: Touch; points: number[] }
    | { t: W.VISIBLE; on: boolean }
    | { t: W.FPS; fps: number }
    /** w, h - the screen of the page in pixels of the device; `now` - else once the window settles. */
    | { t: W.FIT; on: boolean; now: boolean; w: number; h: number }
    | { t: W.SELECT; screen: number }
    | { t: W.ELEVATE }
    | { t: W.SOUND; on: boolean }
    | { t: W.PLAYER; port: MessagePort }
    | { t: W.MIC; port: MessagePort }
    | { t: W.MIC_OFF; tell: boolean }
    | { t: W.CLIPBOARD; text: string }
    | { t: W.FILES; files: File[] }
    | { t: W.FILE_REQUEST }
    | { t: W.FILE_CANCEL }
    | { t: W.TAKE_CONTROL };

export type ToMain =
    | { t: M.PROVED; key: Uint8Array; accepted: boolean }
    /** `error` - why, in the words of the browser. */
    | { t: M.STATUS; error: string }
    /** `error` - why the connect failed, in the words of the browser; "" - the reason went by `STATUS`, or there is none. */
    | { t: M.DIALED; ok: boolean; error: string }
    | { t: M.WELCOME; host: string | undefined; administrator: boolean }
    /** `code` - the number of the reason the machine sent; `left` - the attempts left, -1 - not told; `forget` - the token was wrong: it is not kept. */
    | { t: M.DENIED; reason: Refusal; code: number; left: number; forget: boolean }
    /** `watchers` - the viewers in, this one among them; `yours` - this one controls; `controller` - the name of the one that does. */
    | { t: M.AUDIENCE; watchers: number; yours: boolean; controller: string }
    | { t: M.CLOSED }
    | { t: M.NOTICE; elevation: Elevation }
    | { t: M.ADMIN; admin: Admin }
    | { t: M.SIZE; w: number; h: number }
    | { t: M.SCREEN; screen: number }
    /** `active` - the display the user of the machine works on, -1 - not told. */
    | { t: M.DISPLAYS; displays: Display[]; active: number }
    /** `rgba` - w × h pixels, the hot spot at x, y. */
    | { t: M.CURSOR; w: number; h: number; x: number; y: number; rgba: ArrayBuffer }
    | { t: M.HOST_AT; x: number; y: number; screen: number }
    /** `latency` - from the input to the frame that shows it, ms; -1 - no input yet. */
    | { t: M.STATS; w: number; h: number; fps: number; mbps: number; capture: number; encode: number; latency: number }
    | { t: M.CLIPBOARD; text: string }
    /**
     * `name` - of the file, "" - not known yet; `n` - the percent ( PROGRESS ) or the bytes ( RECEIVED ); `more` - files waiting after it;
     * `code` - the error of the system of the machine, -1 - none; `busy` - a transfer goes on: it can be cancelled.
     */
    | { t: M.FILE_STATUS; line: FileLine; name: string; n: number; more: number; why: FileWhy; code: number; busy: boolean }
    | { t: M.DOWNLOAD; blob: Blob; name: string }
    | { t: M.MIC_ERROR };
