// The QR code of the window of rdp-host, read by the camera of this device: the line it shows, as if pasted. The reader of the browser
// ( BarcodeDetector: Chrome on Android, on macOS ) where it reads QR codes; else jsQR, on the frames of the video drawn on a canvas.
// The camera is asked for at the press only, and let go as soon as the code is read or the reading is cancelled.
//
// A page the browser gives no camera - one opened from the Downloads of a phone, content://, is refused it without a word; one of
// http:// - takes a photo instead: the camera app of the phone, needing no permission of the page, and the code read on the picture.
// Without a camera, the same press picks a picture: a screenshot of the window of rdp-host.

import jsQR from "jsqr";

/** Can the page show the camera itself: a secure page, a browser that gives one, and not refused it. */
async function live(): Promise<boolean> {
    if (!window.isSecureContext || typeof navigator.mediaDevices?.getUserMedia !== "function") return false;
    try {
        return (await navigator.permissions.query({ name: "camera" as PermissionName })).state !== "denied";
    } catch {
        return true; // a browser that tells nothing of it: asked
    }
}

/** The reader of the browser, when it reads QR codes; undefined - jsQR then. */
async function nativeReader(): Promise<{ detect(source: CanvasImageSource): Promise<{ rawValue: string }[]> } | undefined> {
    const Detector = (window as unknown as { BarcodeDetector?: { new(o: { formats: string[] }): { detect(s: CanvasImageSource): Promise<{ rawValue: string }[]> }; getSupportedFormats(): Promise<string[]> } }).BarcodeDetector;
    if (Detector === undefined) return undefined;
    try {
        return (await Detector.getSupportedFormats()).includes("qr_code") ? new Detector({ formats: ["qr_code"] }) : undefined;
    } catch {
        return undefined;
    }
}

/** How often a frame is read. */
const EVERY_MS = 150;
/** The frames jsQR reads are no wider than this: enough for a code that fills a part of the picture, and quick on a phone. */
const WIDE = 720;

/** The QR code read: by the camera shown on the page where it may, else on a photo or a picture. Its text; undefined - cancelled.
 *  Throws when it cannot: the message says why. */
export async function scan(): Promise<string | undefined> {
    if (await live()) {
        try {
            return await watch();
        } catch (e) {
            // no camera here: a picture instead. Refused now: the next press takes a photo
            if (e instanceof DOMException && e.name === "NotFoundError") return photo();
            if (e instanceof DOMException && e.name === "NotAllowedError") throw new Error("not allowed for this page: press 📷 again, and take a photo of the code instead");
            throw e;
        }
    }
    return photo();
}

/** A photo by the camera app - a picture, where there is no camera -, the code read on it. */
function photo(): Promise<string | undefined> {
    const input = document.createElement("input");
    input.type = "file";
    input.accept = "image/*";
    input.capture = "environment";
    return new Promise<string | undefined>((resolve, reject) => {
        input.addEventListener("cancel", () => resolve(undefined));
        input.addEventListener("change", async () => {
            const file = input.files?.[0];
            if (file === undefined) return resolve(undefined);
            try {
                const text = await readPicture(file);
                if (text === undefined) reject(new Error("no QR code on the picture: take it again, the code larger in it and sharp"));
                else resolve(text);
            } catch (e) {
                reject(e);
            }
        });
        input.click();
    });
}

/** The text of the QR code on a picture; undefined - none seen. A photo of a phone is large: jsQR reads it made smaller. */
async function readPicture(file: Blob): Promise<string | undefined> {
    const picture = await createImageBitmap(file);
    try {
        const reader = await nativeReader();
        if (reader !== undefined) return (await reader.detect(picture))[0]?.rawValue.trim() || undefined;
        for (const wide of [1600, 1000, 640]) {
            const scale = Math.min(1, wide / picture.width);
            const canvas = document.createElement("canvas");
            canvas.width = Math.round(picture.width * scale);
            canvas.height = Math.round(picture.height * scale);
            const pen = canvas.getContext("2d", { willReadFrequently: true })!;
            pen.drawImage(picture, 0, 0, canvas.width, canvas.height);
            const frame = pen.getImageData(0, 0, canvas.width, canvas.height);
            const text = jsQR(frame.data, frame.width, frame.height, { inversionAttempts: "attemptBoth" })?.data.trim();
            if (text) return text;
        }
        return undefined;
    } finally {
        picture.close();
    }
}

/**
 * Shows the camera over the page and reads until a QR code is seen: its text; undefined - cancelled. Throws when there is no camera,
 * or it is not allowed.
 */
async function watch(): Promise<string | undefined> {
    const stream = await navigator.mediaDevices.getUserMedia({ video: { facingMode: { ideal: "environment" }, width: { ideal: 1280 }, height: { ideal: 720 } }, audio: false });
    const shade = document.createElement("div");
    shade.id = "scanner";
    const video = document.createElement("video");
    video.muted = true;
    video.playsInline = true;
    video.srcObject = stream;
    const say = document.createElement("p");
    say.textContent = "Point the camera at the QR code in the window of rdp-host";
    const cancel = document.createElement("button");
    cancel.type = "button";
    cancel.textContent = "Cancel";
    shade.append(video, say, cancel);
    document.body.append(shade);

    const reader = await nativeReader();
    const canvas = document.createElement("canvas");
    const pen = canvas.getContext("2d", { willReadFrequently: true })!;
    let timer = 0;
    return new Promise<string | undefined>(resolve => {
        const end = (text: string | undefined) => {
            clearTimeout(timer);
            for (const t of stream.getTracks()) t.stop();
            shade.remove();
            removeEventListener("keydown", escape);
            resolve(text);
        };
        const escape = (e: KeyboardEvent) => { if (e.key === "Escape") end(undefined); };
        addEventListener("keydown", escape);
        cancel.addEventListener("click", () => end(undefined));
        const read = async () => {
            if (!shade.isConnected) return;
            let text: string | undefined;
            if (video.readyState >= video.HAVE_CURRENT_DATA && video.videoWidth > 0) {
                if (reader !== undefined) {
                    text = (await reader.detect(video).catch(() => []))[0]?.rawValue;
                } else {
                    const scale = Math.min(1, WIDE / video.videoWidth);
                    canvas.width = Math.round(video.videoWidth * scale);
                    canvas.height = Math.round(video.videoHeight * scale);
                    pen.drawImage(video, 0, 0, canvas.width, canvas.height);
                    const frame = pen.getImageData(0, 0, canvas.width, canvas.height);
                    // the window may be light on dark or dark on light: both are tried
                    text = jsQR(frame.data, frame.width, frame.height, { inversionAttempts: "attemptBoth" })?.data;
                }
            }
            if (text !== undefined && text !== "") end(text.trim());
            else if (shade.isConnected) timer = window.setTimeout(read, EVERY_MS);
        };
        video.play().then(read, () => end(undefined));
    });
}
