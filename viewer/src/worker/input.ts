// The input of the page in the worker: the messages of ../input.ts made into packs for the machine. The page has worked out the
// places in fractions of the canvas; the display they are on, and where a click of a locked pointer goes, are known here.

import Viewer from "../../protocol/gen/Viewer";
import Session from "../../protocol/gen/Session/Connection";
import Actor0 from "../../protocol/gen/Session/Actor0";
import { W, TOUCH_POINT, type ToWorker } from "../messages";
import type { Video } from "./video";

const V = Viewer.Viewer_;
const send = () => Actor0.State.Working.ONE.transmitter;
const clamp = (v: number) => Math.max(-32768, Math.min(32767, v));
const buttons = [V.MouseButton.Button.LEFT, V.MouseButton.Button.MIDDLE, V.MouseButton.Button.RIGHT, V.MouseButton.Button.BACK, V.MouseButton.Button.FORWARD];
/** By `Touch` of ../messages.ts. */
const kinds = [V.TouchEvent.Kind.START, V.TouchEvent.Kind.MOVE, V.TouchEvent.Kind.END, V.TouchEvent.Kind.CANCEL];

export type Message = Extract<ToWorker, { t: W.POINTER | W.STAMP | W.KEY | W.TEXT | W.BUTTON | W.TOUCH }>;

/** The page's `performance.now()` + `shift` - the worker's: the machine returns the time of the input, the worker measures by it. */
let shift = 0;

/** `page` - `performance.timeOrigin` of the page. */
export function origin(page: number): void {
    shift = page - performance.timeOrigin;
}

/** Sends `m` to the machine on `conn`; `shown` - the video, its display and the pointer of the machine. */
export function apply(m: Message, conn: Session, shown: Video): void {
    switch (m.t) {
        case W.POINTER:
            if (m.moved) send().send_Viewer_MouseMove(Viewer.FractionalCoordinate.New.of(m.x, m.y, shown.screen()), conn);
            if (m.dx || m.dy) send().send_Viewer_MouseDelta(V.MouseDelta.New.of(clamp(m.dx), clamp(m.dy)), conn);
            if (m.wx || m.wy) send().send_Viewer_MouseWheel(V.MouseWheel.New.of(clamp(m.wx), clamp(m.wy)), conn);
            return;
        case W.STAMP:
            send().send_Viewer_EventTime(Math.floor(m.ms + shift) >>> 0, conn);
            return;
        case W.KEY: {
            let pack = V.KeyEvent.EMPTY_PACK;
            pack = V.KeyEvent.set.pressed(pack, m.pressed);
            pack = V.KeyEvent.set.usb_keycode(pack, m.usb);
            if (m.caps !== undefined) pack = V.KeyEvent.set.caps_lock_state(pack, m.caps);
            if (m.num !== undefined) pack = V.KeyEvent.set.num_lock_state(pack, m.num);
            send().send_Viewer_KeyEvent(pack, conn);
            return;
        }
        case W.TEXT: {
            const t = new V.TextEvent();
            t.text = m.text;
            send().send_Viewer_TextEvent(t, conn);
            return;
        }
        case W.BUTTON: {
            const b = buttons[m.button];
            if (b === undefined) return;
            const at = Viewer.FractionalCoordinate.New.of(m.x, m.y, shown.screen());
            send().send_Viewer_MouseButton(V.MouseButton.New.of(b, m.down, m.locked ? shown.hostAt() ?? at : at), conn);
            return;
        }
        case W.TOUCH: {
            const t = new V.TouchEvent();
            t.kind = kinds[m.kind] ?? V.TouchEvent.Kind.CANCEL;
            const points: Viewer.Viewer_.TouchPoint[] = [];
            const n = m.points, screen = shown.screen();
            for (let i = 0; i + TOUCH_POINT <= n.length; i += TOUCH_POINT) {
                const p = new V.TouchPoint();
                p.id = n[i];
                p.at = Viewer.FractionalCoordinate.New.of(n[i + 1], n[i + 2], screen);
                p.radius_x = n[i + 3];
                p.radius_y = n[i + 4];
                p.angle = n[i + 5];
                p.pressure = n[i + 6];
                points.push(p);
            }
            t.points = points;
            send().send_Viewer_TouchEvent(t, conn);
            return;
        }
    }
}
