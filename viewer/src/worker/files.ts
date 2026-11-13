// Files between the page and the machine, in the worker, one transfer at a time: the protocol does not tell transfers apart.
//
// To the machine: the files the page posts go one after another - `Metadata`, `Data` in chunks of 64 KiB read from the File here,
// `Done` - no more than `AHEAD` chunks wait to be sent, so the input is not held behind a file. The machine writes each to the
// Downloads folder of its user and answers `Success` or `Error`.
// From the machine: the page asks, the user of the machine picks the file there, the chunks are gathered here and the page saves
// the whole of it. How it goes is told to the page by codes: the page says it in its words.

import Viewer from "../../protocol/gen/Viewer";
import Session from "../../protocol/gen/Session/Connection";
import Actor0 from "../../protocol/gen/Session/Actor0";
import { M, FileLine, FileWhy } from "../messages";
import { toPage } from "./page";

const Working = Actor0.State.Working;
const F = Viewer.FileTransfer;
const CHUNK = 64 * 1024;
const AHEAD = 2; // 128 KiB at most in front of a key or an acknowledgement of a frame

export interface Files {
    send(files: File[]): void;
    /** Asks the machine for a file: its user picks it. */
    request(): void;
    /** Stops the transfer going on. */
    cancel(): void;
    stop(): void;
}

const kinds: Record<number, FileWhy> = {
    [F.Error.Kind.CANCELED]: FileWhy.CANCELLED,
    [F.Error.Kind.PROTOCOL_ERROR]: FileWhy.PROTOCOL,
    [F.Error.Kind.PERMISSION_DENIED]: FileWhy.PERMISSION,
    [F.Error.Kind.OUT_OF_DISK_SPACE]: FileWhy.DISK,
    [F.Error.Kind.IO_ERROR]: FileWhy.IO,
};

/** Why a transfer failed: `code` - the error of the system of the machine, -1 - none. */
interface Failure { why: FileWhy; code: number }
const CLOSED: Failure = { why: FileWhy.CLOSED, code: -1 };
const CANCELLED: Failure = { why: FileWhy.CANCELLED, code: -1 };
const ENDED: Failure = { why: FileWhy.ENDED, code: -1 };

export function start(conn: Session): Files {
    const queue: File[] = [];
    let stopped = false;

    /** The transfer going on. */
    let upload: { name: string; answer: (failure: Failure | undefined) => void; cancelled: boolean } | undefined;
    let download: { name: string; size: number; chunks: Uint8Array[]; got: number } | "asked" | undefined;

    /** The line the page shows now: told again with the files waiting after it. */
    let line = FileLine.PROGRESS, lineName = "", lineN = 0, lineWhy = FileWhy.OTHER, lineCode = -1;
    const show = (kind: FileLine, name = "", n = 0, more = 0, failure?: Failure) => {
        line = kind;
        lineName = name;
        lineN = n;
        lineWhy = failure?.why ?? FileWhy.OTHER;
        lineCode = failure?.code ?? -1;
        toPage({ t: M.FILE_STATUS, line, name, n, more, why: lineWhy, code: lineCode, busy: upload !== undefined || download !== undefined });
    };

    // ── the flow of chunks ──

    let sent = 0;
    let wake: (() => void) | undefined;
    const onChunkSent = (_pack: Viewer.FileTransfer.Data, c: Session) => {
        if (c !== conn) return;
        sent++;
        wake?.();
        wake = undefined;
    };
    const tick = () => new Promise<void>(resolve => { wake = resolve; setTimeout(resolve, 50); });
    const put = async (send: () => boolean) => {
        while (!send()) {
            if (stopped || !conn.ext_connection.isOpen) return false;
            await new Promise(resolve => setTimeout(resolve, 10));
        }
        return true;
    };

    // ── to the machine ──

    const next = async () => {
        if (upload || download || stopped) return;
        const file = queue.shift();
        if (!file) return;

        let answered: (failure: Failure | undefined) => void = () => undefined;
        const answer = new Promise<Failure | undefined>(resolve => { answered = resolve; });
        const current = { name: file.name, answer: answered, cancelled: false };
        upload = current;

        const meta = new F.Metadata();
        meta.filename = file.name;
        meta.size = file.size;
        show(FileLine.PROGRESS, file.name, 0, queue.length);
        let error = await put(() => Working.ONE.transmitter.send_FileTransfer_Metadata(meta, conn)) ? undefined : CLOSED;

        const base = sent;
        let queued = 0;
        for (let offset = 0; error === undefined && offset < file.size; offset += CHUNK) {
            while (queued - (sent - base) >= AHEAD && !current.cancelled && !stopped) await tick();
            if (current.cancelled || stopped) break;
            const bytes = new Uint8Array(await file.slice(offset, offset + CHUNK).arrayBuffer());
            const data = new F.Data();
            data.data_new(bytes.length).view().set(bytes);
            if (!await put(() => Working.ONE.transmitter.send_FileTransfer_Data(data, conn))) error = CLOSED;
            queued++;
            show(FileLine.PROGRESS, file.name, Math.floor((offset + bytes.length) / Math.max(1, file.size) * 100), queue.length);
        }
        if (error === undefined && !current.cancelled && !stopped) {
            if (!await put(() => Working.ONE.transmitter.send_FileTransfer_Done(conn))) error = CLOSED;
        }
        const result = error ?? await answer;
        if (upload === current) upload = undefined;
        if (!stopped) {
            if (result === undefined) show(FileLine.SAVED, file.name);
            else show(FileLine.FAILED, file.name, 0, 0, result);
            void next();
        }
    };

    // ── from the machine ──

    const onMetadata = (meta: Viewer.FileTransfer.Metadata, c: Session) => {
        if (c !== conn) return;
        download = { name: meta.filename || "file", size: meta.size, chunks: [], got: 0 };
        show(FileLine.PROGRESS, download.name);
    };

    const onData = (data: Viewer.FileTransfer.Data, c: Session) => {
        if (c !== conn || typeof download !== "object") return;
        const bytes = data.data?.view();
        if (!bytes) return;
        download.chunks.push(bytes);
        download.got += bytes.length;
        show(FileLine.PROGRESS, download.name, Math.floor(download.got / Math.max(1, download.size) * 100));
    };

    const onDone = (c: Session) => {
        if (c !== conn || typeof download !== "object") return;
        const { name, chunks, got } = download;
        download = undefined;
        toPage({ t: M.DOWNLOAD, blob: new Blob(chunks as BlobPart[]), name });
        Working.ONE.transmitter.send_FileTransfer_Success(conn);
        show(FileLine.RECEIVED, name, got);
        void next();
    };

    // ── either way ──

    const onSuccess = (c: Session) => {
        if (c === conn) upload?.answer(undefined);
    };

    const onError = (error: Viewer.FileTransfer.Error, c: Session) => {
        if (c !== conn) return;
        const failure: Failure = {
            why: kinds[F.Error.get.kind(error)] ?? FileWhy.OTHER,
            code: F.Error.hasValue.api_error_code(error) ? F.Error.get.api_error_code(error) : -1,
        };
        if (upload) return upload.answer(failure);
        if (download) {
            const name = typeof download === "object" ? download.name : ""; // "": the page names the file of the machine
            download = undefined;
            show(FileLine.FAILED, name, 0, 0, failure);
            void next();
        }
    };

    Working.OnReceived.FileTransfer_Metadata.handlers.subscribe(onMetadata);
    Working.OnReceived.FileTransfer_Data.handlers.subscribe(onData);
    Working.OnReceived.FileTransfer_Done.handlers.subscribe(onDone);
    Working.OnReceived.FileTransfer_Success.handlers.subscribe(onSuccess);
    Working.OnReceived.FileTransfer_Error.handlers.subscribe(onError);
    Working.OnSerialized.FileTransfer_Data.handlers.subscribe(onChunkSent);

    return {
        send(files) {
            queue.push(...files);
            if (upload || download) show(line, lineName, lineN, queue.length, { why: lineWhy, code: lineCode });
            void next();
        },
        request() {
            if (upload || download) return show(FileLine.WAIT);
            download = "asked";
            Working.ONE.transmitter.send_FileTransfer_RequestTransfer(conn);
            show(FileLine.PICKING);
        },
        cancel() {
            queue.length = 0;
            if (!upload && !download) return;
            Working.ONE.transmitter.send_FileTransfer_Error(F.Error.set.kind(F.Error.EMPTY_PACK, F.Error.Kind.CANCELED), conn);
            if (upload) {
                upload.cancelled = true;
                upload.answer(CANCELLED);
            }
            if (download) {
                download = undefined;
                show(FileLine.CANCELLED);
            }
        },
        stop() {
            stopped = true;
            wake?.();
            upload?.answer(ENDED);
            queue.length = 0;
            Working.OnReceived.FileTransfer_Metadata.handlers.unsubscribe(onMetadata);
            Working.OnReceived.FileTransfer_Data.handlers.unsubscribe(onData);
            Working.OnReceived.FileTransfer_Done.handlers.unsubscribe(onDone);
            Working.OnReceived.FileTransfer_Success.handlers.unsubscribe(onSuccess);
            Working.OnReceived.FileTransfer_Error.handlers.unsubscribe(onError);
            Working.OnSerialized.FileTransfer_Data.handlers.unsubscribe(onChunkSent);
        },
    };
}
