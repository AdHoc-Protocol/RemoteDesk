using org.unirail.Meta;

// A remote desktop: a browser page sees and controls the desktop of a Windows machine. The messages come from Chrome Remote
// Desktop ( remoting/proto: control, event, video, audio, file_transfer, coordinates, error_code ), made over for one machine host
// in Rust and one viewer page in TypeScript, and cut to what they use: each message is a pack of its own, the pack id tells which
// one came; what WebRTC carried for CRD - video, sound, the login, the bitrate - is carried here, on one WebSocket, under Noise,
// the bitrate fitted to the link by the machine itself.
namespace org.unirail.RemoteDesk
{
    /** packs
        <see cref='AudioPacket'/>ā                     24
        <see cref='ClipboardEvent'/>Ā                  23
        <see cref='FileTransfer'/>Ă
        <see cref='FileTransfer.Data'/>Ą               26
        <see cref='FileTransfer.Done'/>ą               27
        <see cref='FileTransfer.Error'/>Ĉ              30
        <see cref='FileTransfer.Error.Kind'/>ĳ
        <see cref='FileTransfer.Metadata'/>ă           25
        <see cref='FileTransfer.RequestTransfer'/>ć    29
        <see cref='FileTransfer.Success'/>Ć            28
        <see cref='FractionalCoordinate'/>ÿ
        <see cref='Machine.ActiveDisplay'/>ħ           33
        <see cref='Machine.Audience'/>ě                19
        <see cref='Machine.CursorShape'/>Ĩ             34
        <see cref='Machine.Denied'/>ġ                  3
        <see cref='Machine.Display'/>Ħ
        <see cref='Machine.Elevation'/>ċ               2
        <see cref='Machine.Elevation.State'/>Ğ
        <see cref='Machine.HostCursorPosition'/>ĩ      35
        <see cref='Machine.MicrophoneError'/>ĭ         37
        <see cref='Machine.VideoFormat'/>Ĥ
        <see cref='Machine.VideoLayout'/>ĥ             32
        <see cref='Machine.VideoPacket'/>ģ             31
        <see cref='Machine.Welcome'/>Ġ                 5
        <see cref='Reason'/>į
        <see cref='Viewer.AudioControl'/>Ę             16
        <see cref='Viewer.ClientResolution'/>ĕ         14
        <see cref='Viewer.ElevationRequest'/>Ċ         1
        <see cref='Viewer.EventTime'/>Ĕ                13
        <see cref='Viewer.Hello'/>ĉ                    0
        <see cref='Viewer.KeyEvent'/>Č                 6
        <see cref='Viewer.KeyframeRequest'/>ĝ          21
        <see cref='Viewer.Mark'/>ğ                     22
        <see cref='Viewer.Mark.Kind'/>Ģ
        <see cref='Viewer.MicrophoneControl'/>ę        17
        <see cref='Viewer.MouseButton'/>ď              9
        <see cref='Viewer.MouseButton.Button'/>Ĵ
        <see cref='Viewer.MouseDelta'/>Đ               10
        <see cref='Viewer.MouseMove'/>Ď                8
        <see cref='Viewer.MouseWheel'/>đ               11
        <see cref='Viewer.SelectDisplay'/>Ě            18
        <see cref='Viewer.TakeControl'/>ė              4
        <see cref='Viewer.TextEvent'/>č                7
        <see cref='Viewer.TouchEvent'/>Ē               12
        <see cref='Viewer.TouchEvent.Kind'/>ĵ
        <see cref='Viewer.TouchPoint'/>ē
        <see cref='Viewer.VideoAck'/>Ĝ                 20
        <see cref='Viewer.VideoControl'/>Ė             15
    */
    /** project
        <see cref='RemoteDesk'/>ĆěęĿĎ

        hosts
        <see cref='Viewer'/>ÿ
        <see cref='Machine'/>Ā

        connections
        <see cref='Session'/>ÿ

        states
        <see cref='Session.Granting'/>Ă
        <see cref='Session.Start'/>ÿ
        <see cref='Session.Working'/>ă
    */
    public interface RemoteDesk
    {
        // ═════════════════════════ shared ═════════════════════════

        /// <summary>
        /// A point on a display: x and y as a fraction of its width and height, 0 … 65535 for [0, 1]; the display - its number in
        /// the last `VideoLayout` the machine sent. Precise to a pixel and a half on a 100 000-pixel wide display.
        /// </summary>
        public class FractionalCoordinate
        {
            ushort                   x;
            ushort                   y;
            [MinMax(0, 15)] byte     screen;
        }

        /// <summary>Why the machine refuses a login.</summary>
        public enum Reason
        {
            /// <summary>An error that is none of the ones below.</summary>
            UNEXPECTED,
            /// <summary>A wrong token.</summary>
            AUTHENTICATION_FAILED,
            /// <summary>Too many failed logins: the machine takes none for a while.</summary>
            HOST_OVERLOAD,
            /// <summary>The machine has as many viewers as it takes.</summary>
            FULL,
        }

        /// <summary>
        /// The clipboard: the text copied on one side goes to the other. UTF-8, lines end in `\n`.
        /// </summary>
        public class ClipboardEvent
        {
            /// <summary>The text: as the machine takes at most, 1 MiB.</summary>
            [D(1_048_576)] Binary[,,] data;
        }

        /// <summary>
        /// One packet of sound, 20 ms: the machine's to the viewer, 48 kHz stereo; the viewer's microphone to the machine, 48 kHz mono.
        /// 16-bit signed samples, little endian, the channels interleaved.
        /// </summary>
        public class AudioPacket
        {
            [D(65_536)] Binary[,,]       data;
        }

        /// <summary>
        /// A file from one side to the other: `Metadata`, then `Data` in chunks, then `Done`; the receiver answers `Success` or `Error`.
        /// Either side may send `Error` at any time: the transfer is over, what is already on the wire is dropped.
        /// </summary>
        public class FileTransfer
        {
            /// <summary>Always before any `Data`.</summary>
            public class Metadata
            {
                [D(+255)] string filename;

                /// <summary>Bytes. A file may turn out longer or shorter: `Done` ends it, not the count.</summary>
                ulongJS size;
            }

            /// <summary>A chunk of the file.</summary>
            public class Data { [D(65_536)] Binary[,,] data; }

            /// <summary>After the last chunk: the file is complete.</summary>
            public class Done { }

            /// <summary>The receiver has saved the file.</summary>
            public class Success { }

            /// <summary>The viewer asks for a file of the machine: the user of the machine picks it.</summary>
            public class RequestTransfer { }

            /// <summary>The transfer is aborted: by an error or by the user.</summary>
            public class Error
            {
                public enum Kind { CANCELED, PROTOCOL_ERROR, PERMISSION_DENIED, OUT_OF_DISK_SPACE, IO_ERROR, }

                /// <summary>Selects the message the user sees.</summary>
                Kind kind;

                /// <summary>The error code of the system call that failed.</summary>
                int? api_error_code;
            }
        }

        // ═════════════════════════ the viewer ═════════════════════════

        /// <summary>
        /// The page in the browser: shows the desktop of the machine and controls it - keyboard, mouse, touch, clipboard, files,
        /// microphone.
        /// </summary>
        /**
        <see cref="InTS"/>--
        <see cref='Machine.VideoPacket'/>
        <see cref='AudioPacket'/>
        <see cref="InTS"/>
        */
        struct Viewer : Host
        {
            // ── login ──

            /// <summary>
            /// The login: the token the machine expects. How the token gets from the machine to the user - a message, a mail, anything -
            /// is not the protocol's; it travels here inside Noise.
            /// </summary>
            public class Hello
            {
                [D(+256)] string token;
                [D(+64)]  string client_name; // the name the user of the machine sees
            }

            // ── input ──

            /// <summary>
            /// A key went down or up. The USB HID usage of the physical key: the page in the upper 16 bits ( 0x07 keyboard, 0x0C
            /// consumer ), the usage in the lower; the layout is the machine's.
            /// </summary>
            public class KeyEvent
            {
                bool                           pressed;
                [MinMax(0, 0x0C_FFFF)] uint    usb_keycode;

                /// <summary>The lock states to set before the key: only those the viewer knows for sure ( macOS has no Num Lock ).</summary>
                bool? caps_lock_state;
                bool? num_lock_state;
            }

            /// <summary>Text of an input method other than a physical keyboard: a soft keyboard, gesture typing, voice input.</summary>
            public class TextEvent { [D(+1_024)] string text; }

            /// <summary>The pointer moved: a point on a display.</summary>
            public class MouseMove { FractionalCoordinate at; }

            public class MouseButton
            {
                public enum Button { LEFT, MIDDLE, RIGHT, BACK, FORWARD, }

                Button               button;
                bool                 down;
                FractionalCoordinate at;
            }

            /// <summary>The pointer moved by this much, in pixels of the viewer: the pointer is locked in the page.</summary>
            public class MouseDelta
            {
                short dx;
                short dy;
            }

            /// <summary>
            /// The wheel, in 120ths of a notch, as the event would scroll on the viewer: positive - away from the user, up or right.
            /// </summary>
            public class MouseWheel
            {
                short x;
                short y;
            }

            public class TouchEvent
            {
                /// <summary>
                /// START - the points just added; MOVE - the started points moved; END - they ended; CANCEL - they were canceled, as
                /// END but the machine may not act on them the same way.
                /// </summary>
                public enum Kind { START, MOVE, END, CANCEL, }

                Kind                      kind;

                /// <summary>Only the points that changed: one for START, END and CANCEL as a rule, several for MOVE.</summary>
                [D(10)] TouchPoint[,,]    points;
            }

            public class TouchPoint
            {
                [MinMax(0, 31)] byte          id;
                FractionalCoordinate          at;
                [MinMax(0, 4095)] ushort      radius_x; // pixels of the machine
                [MinMax(0, 4095)] ushort      radius_y;
                [MinMax(0, 359)] ushort       angle;    // degrees from the y-axis
                byte                          pressure; // 0 … 255 for 0 … 1
            }

            /// <summary>
            /// Milliseconds of the viewer's clock, sent before a burst of input: the machine returns the last one in
            /// `VideoPacket.latest_event_ms` of the first frame captured after it - the viewer sees how long input takes to show.
            /// </summary>
            public class EventTime { uint ms; }

            // ── marks ──

            /// <summary>
            /// A mark the viewer draws over the screen of the machine for its user - where to tap, which field: it shows at once and
            /// fades by itself. Sent as it is drawn: each pack carries the points added to its stroke since the last one. Any viewer
            /// draws, not only the one that controls.
            /// </summary>
            public class Mark
            {
                /// <summary>LINE - drawn by hand; RING - a ring about the first point, a tap of the viewer: here; ARROW - from the first point to the last.</summary>
                public enum Kind { LINE, RING, ARROW, }

                Kind                      kind;
                /// <summary>Numbers the strokes of the viewer: a pack of another number begins another stroke.</summary>
                byte                      stroke;
                /// <summary>The stroke is drawn: it fades from now.</summary>
                bool                      done;
                /// <summary>Its colour, of the few the page offers: 0 red, 1 yellow, 2 green, 3 blue.</summary>
                [MinMax(0, 3)] byte       color;
                [MinMax(0, 15)] byte      screen;
                /// <summary>The points added: x, y, x, y … as fractions of the display, 0 … 65535 for [0, 1].</summary>
                [D(512)] ushort[,,]       points;
            }

            // ── control ──

            /// <summary>
            /// The size of the viewer's window for a display: the machine may change the resolution of that display to fit it. All
            /// zero: the machine restores the resolution it had.
            /// </summary>
            public class ClientResolution
            {
                ushort                    width;  // pixels of the viewer's device
                ushort                    height;
                [MinMax(0, 15)] byte      screen;
            }

            /// <summary>
            /// Video on or off - off while the viewer's page is not seen - and the most frames a second. A field left out stays as it is.
            /// </summary>
            public class VideoControl
            {
                bool?                         enable;
                [MinMax(1, 120)] byte?        framerate;
            }

            /// <summary>The machine's sound on or off.</summary>
            public class AudioControl { bool enable; }

            /// <summary>
            /// The viewer's microphone played on the machine, or not. The newest request holds; when the machine cannot play it, it
            /// answers `MicrophoneError`.
            /// </summary>
            public class MicrophoneControl { bool enable; }

            /// <summary>Which display to show.</summary>
            public class SelectDisplay { [MinMax(0, 15)] byte screen; }

            /// <summary>
            /// A frame was shown. Sent for every frame, in the order the frames came: the machine sends the next frames as the
            /// earlier ones are acknowledged.
            /// </summary>
            public class VideoAck { uint frame_id; }

            /// <summary>The decoder of the viewer needs a key frame: it starts, or lost its state.</summary>
            public class KeyframeRequest { }

            /// <summary>
            /// The viewer takes the control of the machine: from now on its input goes there, and the input of the other viewers does
            /// not. Every viewer sees the screen; one controls it. The machine answers every viewer with `Audience`.
            /// </summary>
            public class TakeControl { }

            // ── rights ──

            /// <summary>
            /// The operator asks for the rights of an administrator: the user of the machine confirms in the prompt of UAC, and the
            /// program of the machine starts again as administrator - its input then reaches the windows of programs run as
            /// administrator. The machine answers `Elevation`.
            /// </summary>
            public class ElevationRequest { }
        }

        // ═════════════════════════ the machine ═════════════════════════

        /// <summary>
        /// The Windows machine whose desktop is seen: captures the displays and the sound, encodes the video, injects the input.
        /// </summary>
        /**
        <see cref="InRS"/>--
        <see cref='Machine.VideoPacket'/>
        <see cref="InRS"/>
        */
        struct Machine : Host
        {
            // ── login ──

            /// <summary>The viewer is in.</summary>
            public class Welcome
            {
                [D(+64)] string host_name;

                /// <summary>The program of the machine runs as administrator.</summary>
                bool administrator;

                /// <summary>
                /// The machine is a phone or a tablet: its input is touches, its buttons Back, Home and Recents ( `KeyEvent` of the
                /// consumer page: AC Back 0x0C0224, AC Home 0x0C0223, AL Select Task 0x0C01A2 ) and text into the field in
                /// focus - the page shows the buttons, and sends the characters typed as `TextEvent`: a phone has no layout of keys.
                /// </summary>
                bool phone;
            }

            /// <summary>The login is refused.</summary>
            public class Denied
            {
                Reason                      reason;
                [MinMax(0, 10)] byte?       attempts_left;
            }

            /// <summary>
            /// Who watches the machine and who controls it, as the viewer it goes to sees it: sent to every viewer when it changes. The
            /// first viewer in controls; when the one that controls leaves, the one that watches longest does.
            /// </summary>
            public class Audience
            {
                [MinMax(1, 16)] byte watchers;   // the viewers in, this one among them
                bool                 yours;      // this viewer controls the machine
                [D(+64)] string      controller; // the name of the viewer that controls
            }

            // ── video ──

            /// <summary>
            /// A frame of a display, as the encoder made it. `format` comes with the first frame of a display and whenever it changes.
            /// </summary>
            public class VideoPacket
            {
                [MinMax(0, 15)] byte           screen;
                uint                           frame_id;

                /// <summary>A key frame: decodes alone.</summary>
                bool                           keyframe;
                VideoFormat?                   format;

                /// <summary>H.264: an access unit in Annex B form, the parameter sets in front of a key frame.</summary>
                [D(16_777_216)] Binary[,,]     data;

                /// <summary>Milliseconds: from the capture to the encoder, in the encoder.</summary>
                ushort capture_ms;
                ushort encode_ms;

                /// <summary>The newest `EventTime.ms` the machine had when this frame was captured.</summary>
                uint? latest_event_ms;
            }

            /// <summary>The frame in pixels of the display.</summary>
            public class VideoFormat
            {
                ushort width;
                ushort height;
            }

            /// <summary>The displays of the machine to choose from.</summary>
            public class VideoLayout
            {
                [D(16)] Display[,,]           displays;
            }

            public class Display
            {
                [MinMax(0, 15)] byte  screen;
                ushort                width;  // pixels
                ushort                height;
                [D(+64)] string       name;
            }

            /// <summary>The display the user of the machine works on now.</summary>
            public class ActiveDisplay { [MinMax(0, 15)] byte screen; }

            /// <summary>The shape of the pointer: the viewer draws it itself, so the pointer moves without waiting for video.</summary>
            public class CursorShape
            {
                [MinMax(1, 256)] ushort     width;
                [MinMax(1, 256)] ushort     height;
                [MinMax(0, 255)] byte       hotspot_x;
                [MinMax(0, 255)] byte       hotspot_y;
                [D(262_144)] Binary[,,]     bgra;   // width * height * 4
            }

            /// <summary>
            /// Where the machine's own pointer is: the viewer draws the pointer there - the pointer is locked in the page, or the user
            /// of the machine moves it. None: the viewer hides it and tracks its own pointer again.
            /// </summary>
            public class HostCursorPosition { FractionalCoordinate? at; }

            /// <summary>The viewer's microphone cannot be played on the machine: its sound device failed.</summary>
            public class MicrophoneError { }

            /// <summary>What became of `ElevationRequest`.</summary>
            public class Elevation
            {
                /// <summary>
                /// ASKING - the prompt of UAC is on the screen of the machine; CONFIRMED - the program of the machine starts again as
                /// administrator, the connection closes and the viewer connects again; DECLINED - the user of the machine said no;
                /// ALREADY - it runs as administrator; FAILED - the prompt could not be shown; NOT_NEEDED - the system of the machine
                /// lets the input reach every window as it is ( Linux, macOS ).
                /// </summary>
                public enum State { ASKING, CONFIRMED, DECLINED, ALREADY, FAILED, NOT_NEEDED, }

                State state;
            }
        }

        // ═════════════════════════ the connection ═════════════════════════

        /// <summary>
        /// The page dials the machine: a WebSocket under Noise XX - everything it carries is encrypted, the token too. Then the
        /// login by the token; then the session.
        /// </summary>
        [Noise(NoiseAttribute.Pattern.XX)]
        interface Session : Connects<Viewer, Machine>
        {
            [L____________<Granting, Viewer.Hello>/*ÿ*/]
            struct Start { }

            [____________R<Working, Machine.Welcome>/*ÿ*/]
            [____________R<Close, Machine.Denied>/*Ā*/]
            struct Granting { }

            /// <summary>The session: input and control from the page, video, layout and pointer from the machine; clipboard, files and sound both ways.</summary>
            [l____________<(Viewer.KeyEvent, Viewer.TextEvent, Viewer.MouseMove, Viewer.MouseButton, Viewer.MouseDelta, Viewer.MouseWheel, Viewer.TouchEvent, Viewer.EventTime, Viewer.Mark,
                            Viewer.ClientResolution, Viewer.VideoControl, Viewer.AudioControl, Viewer.MicrophoneControl, Viewer.SelectDisplay,
                            Viewer.VideoAck, Viewer.KeyframeRequest, Viewer.TakeControl, Viewer.ElevationRequest,
                            ClipboardEvent, AudioPacket,
                            FileTransfer.Metadata, FileTransfer.Data, FileTransfer.Done, FileTransfer.Success, FileTransfer.RequestTransfer, FileTransfer.Error)>/*ā*/]
            [____________r<(Machine.VideoPacket, Machine.VideoLayout, Machine.ActiveDisplay, Machine.CursorShape, Machine.HostCursorPosition,
                            Machine.MicrophoneError, Machine.Elevation, Machine.Audience,
                            ClipboardEvent, AudioPacket,
                            FileTransfer.Metadata, FileTransfer.Data, FileTransfer.Done, FileTransfer.Success, FileTransfer.Error)>/*ÿ*/]
            struct Working { }
        }
    }
}
