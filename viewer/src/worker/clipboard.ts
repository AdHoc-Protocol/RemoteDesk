// The clipboard of the session in the worker: the text the machine copied goes to the page, which sets it on the clipboard of this
// device; the text the page read there goes to the machine when it is new. The page reads and writes the clipboard ( ../clipboard.ts ).

import Viewer from "../../protocol/gen/Viewer";
import Session from "../../protocol/gen/Session/Connection";
import Actor0 from "../../protocol/gen/Session/Actor0";
import { M } from "../messages";
import { toPage } from "./page";

const Working = Actor0.State.Working;
const decoder = new TextDecoder();
const encoder = new TextEncoder();
const EMPTY = new Uint8Array(0);
/** As the machine takes at most. */
const MAX_BYTES = 1 << 20;

export interface Clipboard {
    /** The text on the clipboard of this device: sent to the machine if it is new. */
    offer(text: string): void;
    stop(): void;
}

export function start(conn: Session): Clipboard {
    /** The text both sides have now: what came from the machine is not sent back. */
    let known: string | undefined;

    const onClipboard = (event: Viewer.ClipboardEvent, c: Session) => {
        if (c !== conn) return;
        const text = decoder.decode(event.data?.view() ?? EMPTY);
        known = text;
        toPage({ t: M.CLIPBOARD, text });
    };

    Working.OnReceived.ClipboardEvent.handlers.subscribe(onClipboard);

    return {
        offer(text: string) {
            if (text === known || text === "") return;
            const bytes = encoder.encode(text);
            if (bytes.length > MAX_BYTES) return console.warn(`the clipboard holds ${bytes.length} bytes of text: more than the machine takes`);
            const event = new Viewer.ClipboardEvent();
            event.data_new(bytes.length).view().set(bytes);
            if (Working.ONE.transmitter.send_ClipboardEvent(event, conn)) known = text;
        },
        stop() {
            Working.OnReceived.ClipboardEvent.handlers.unsubscribe(onClipboard);
        },
    };
}
