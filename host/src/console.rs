// The window of the program: what it says, each kind of news with its sign and colour - the banner at the start, the line to send in
// a frame, who comes and goes - and the keys of its menu, read one by one. Colours only in a console: in a log or a pipe the words go
// plain, the signs stay.

use crossterm::style::{Color, Stylize};
use std::fmt::Display;
use std::io::{self, IsTerminal, Write};
use std::sync::OnceLock;

static COLOUR: OnceLock<bool> = OnceLock::new();

/// Colours: a console that takes them, and no NO_COLOR asked.
fn colour() -> bool {
    *COLOUR.get_or_init(|| {
        if !io::stdout().is_terminal() || std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        #[cfg(windows)]
        {
            crossterm::ansi_support::supports_ansi() // turns the colours of the console on, where it can
        }
        #[cfg(not(windows))]
        {
            true
        }
    })
}

/// `text` in `color`, bold or not; plain when the output is no console.
pub fn paint(text: impl Display, color: Color, bold: bool) -> String {
    if !colour() {
        return text.to_string();
    }
    let styled = text.to_string().with(color);
    if bold { styled.bold().to_string() } else { styled.to_string() }
}

/// The kinds of news, by their colour.
#[derive(Clone, Copy)]
pub enum Tone {
    /// done, in
    Good,
    /// something happened
    News,
    /// waited for
    Wait,
    /// worth a look
    Warn,
    /// failed, refused
    Bad,
    /// by the way
    Quiet,
}

/// One piece of news: its sign, its words in the colour of its kind.
pub fn say(sign: &str, tone: Tone, text: impl Display) {
    let (color, bold) = match tone {
        Tone::Good => (Color::Green, true),
        Tone::News => (Color::Cyan, false),
        Tone::Wait => (Color::Yellow, false),
        Tone::Warn => (Color::DarkYellow, false),
        Tone::Bad => (Color::Red, true),
        Tone::Quiet => (Color::DarkGrey, false),
    };
    #[cfg(target_os = "android")]
    crate::os::tell(sign, tone as i32, &paint(text, color, bold)); // the screen of the app: it has no console, so `paint` adds no colour
    #[cfg(not(target_os = "android"))]
    println!(" {sign} {}", paint(text, color, bold));
}

/// `say` with the words formatted: `say!("👋", Quiet, "{name} left")`.
#[macro_export]
macro_rules! say {
    ($sign:expr, $tone:ident, $($words:tt)*) => {
        $crate::console::say($sign, $crate::console::Tone::$tone, format!($($words)*))
    };
}

// ── the banner ──

const BANNER: [&str; 6] = [
    "██████╗ ███████╗███╗   ███╗ ██████╗ ████████╗███████╗██████╗ ███████╗███████╗██╗  ██╗",
    "██╔══██╗██╔════╝████╗ ████║██╔═══██╗╚══██╔══╝██╔════╝██╔══██╗██╔════╝██╔════╝██║ ██╔╝",
    "██████╔╝█████╗  ██╔████╔██║██║   ██║   ██║   █████╗  ██║  ██║█████╗  ███████╗█████╔╝ ",
    "██╔══██╗██╔══╝  ██║╚██╔╝██║██║   ██║   ██║   ██╔══╝  ██║  ██║██╔══╝  ╚════██║██╔═██╗ ",
    "██║  ██║███████╗██║ ╚═╝ ██║╚██████╔╝   ██║   ███████╗██████╔╝███████╗███████║██║  ██╗",
    "╚═╝  ╚═╝╚══════╝╚═╝     ╚═╝ ╚═════╝    ╚═╝   ╚══════╝╚═════╝ ╚══════╝╚══════╝╚═╝  ╚═╝",
];

/// The colours the banner goes through, left to right: the sky blue of the screen, a violet, the green of a live line.
const GRADIENT: [(u8, u8, u8); 3] = [(79, 195, 247), (149, 117, 255), (92, 194, 126)];

fn blend(at: f32) -> (u8, u8, u8) {
    let at = at.clamp(0.0, 1.0) * (GRADIENT.len() - 1) as f32;
    let i = (at as usize).min(GRADIENT.len() - 2);
    let (a, b, t) = (GRADIENT[i], GRADIENT[i + 1], at - i as f32);
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    (mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

/// The name of the program in big letters, its colours running from left to right, the shadows darker; a narrow window or no
/// console - the name in one line.
pub fn banner() {
    let wide = BANNER[0].chars().count() + 2;
    let fits = crossterm::terminal::size().map(|(w, _)| w as usize >= wide).unwrap_or(false);
    println!();
    if colour() && fits {
        for row in BANNER {
            let mut out = String::from(" ");
            let n = row.chars().count().max(1) as f32;
            for (x, c) in row.chars().enumerate() {
                let (r, g, b) = blend(x as f32 / n);
                let shadow = c != '█' && c != ' ';
                let color = if shadow { Color::Rgb { r: r / 2, g: g / 2, b: b / 2 } } else { Color::Rgb { r, g, b } };
                out += &c.to_string().with(color).to_string();
            }
            println!("{out}");
        }
    } else {
        println!(" {}", paint("RemoteDesk", Color::Cyan, true));
    }
    println!(" {}", paint("   your own remote desktop: see and control this computer from a browser, no company in between", Color::DarkGrey, false));
    println!();
}

// ── the line to send ──

/// The line for whoever connects, on the screen of the app of the phone.
#[cfg(target_os = "android")]
pub fn line_to_send(line: &str) { crate::os::line(line) } // the app shows it, puts it in the clipboard, offers to share it

/// The line for whoever connects, in a frame, and in the clipboard - said so: pasting it into a message is all that is left.
#[cfg(not(target_os = "android"))]
pub fn line_to_send(line: &str) {
    println!();
    say("📨", Tone::News, "Send this line to whoever connects:");
    let bar = "─".repeat(line.chars().count() + 4);
    let frame = |s: &str| paint(s, Color::DarkGreen, false);
    println!("    {}", frame(&format!("╭{bar}╮")));
    println!("    {}{}{}", frame("│  "), paint(line, Color::Green, true), frame("  │"));
    println!("    {}", frame(&format!("╰{bar}╯")));
    if crate::clipboard::put_line(line) {
        say("📋", Tone::Good, "It is in your clipboard already: just paste it into a message.");
    } else {
        say("✂️", Tone::Warn, "Select it with the mouse and copy it: this computer has no clipboard to put it in.");
    }
    qr(line);
    println!();
}

/// The line as a QR code, for a phone beside the computer: the page scans it, 📷 beside its field. Dark on light whatever the colours
/// of the window. Only in a console wide enough: in a log it is no use.
#[cfg(not(target_os = "android"))]
fn qr(line: &str) {
    if !colour() {
        return;
    }
    let Some(rows) = qr_rows(line) else { return };
    if !crossterm::terminal::size().is_ok_and(|(w, _)| w as usize >= rows[0].chars().count() + 4) {
        return;
    }
    say("📷", Tone::News, "Or scan it with a phone: 📷 beside the field of the line on the page.");
    for row in rows {
        println!("    {}", row.with(Color::Black).on(Color::White));
    }
}

/// The rows of the QR code of `line`, a light margin around it: two rows of modules in one row of the window - ▀ ▄ █, dark the
/// colour of the letters.
#[cfg(not(target_os = "android"))]
fn qr_rows(line: &str) -> Option<Vec<String>> {
    use qrcode::{Color as Module, EcLevel, QrCode};
    // the light margin a reader needs around the code
    const QUIET: usize = 2;
    let code = QrCode::with_error_correction_level(line.as_bytes(), EcLevel::L).ok()?;
    let side = code.width();
    let full = side + 2 * QUIET;
    let modules = code.to_colors();
    let dark = |x: usize, y: usize| {
        (QUIET..QUIET + side).contains(&x) && (QUIET..QUIET + side).contains(&y) && modules[(y - QUIET) * side + x - QUIET] == Module::Dark
    };
    Some(
        (0..full)
            .step_by(2)
            .map(|y| {
                (0..full)
                    .map(|x| match (dark(x, y), dark(x, y + 1)) {
                        (true, true) => '█',
                        (true, false) => '▀',
                        (false, true) => '▄',
                        (false, false) => ' ',
                    })
                    .collect()
            })
            .collect(),
    )
}

#[cfg(all(test, not(target_os = "android")))]
mod tests {
    /// The rows read back by a reader of QR codes: each character two modules, 4 pixels a module.
    #[test]
    fn qr_reads_back() {
        let line = "router.example.org:41933/K7QM2XHP9RTA4WZD8CNE-24NDRCK9";
        let rows: Vec<Vec<char>> = super::qr_rows(line).unwrap().iter().map(|r| r.chars().collect()).collect();
        let (w, h) = (rows[0].len(), rows.len() * 2);
        let dark = |x: usize, y: usize| match rows[y / 2][x] {
            '█' => true,
            '▀' => y % 2 == 0,
            '▄' => y % 2 == 1,
            _ => false,
        };
        let mut image = rqrr::PreparedImage::prepare_from_greyscale(w * 4, h * 4, |x, y| if dark(x / 4, y / 4) { 0 } else { 255 });
        let grids = image.detect_grids();
        assert_eq!(grids.len(), 1);
        assert_eq!(grids[0].decode().unwrap().1, line);
    }
}

/// Said where the program waits for whoever connects: a line used later, hours or days on, finds the machine only awake.
pub fn stay_awake() {
    #[cfg(windows)]
    const HOW: &str = "Windows: Settings > System > Power, sleep Never";
    #[cfg(target_os = "macos")]
    const HOW: &str = "macOS: System Settings > Energy, or Battery > Options: keep it from sleeping";
    #[cfg(target_os = "android")]
    const HOW: &str = "the phone: keep it charged, and do not turn it off";
    #[cfg(not(any(windows, target_os = "macos", target_os = "android")))]
    const HOW: &str = "the settings of power of this system: suspend never";
    say!("💤", Warn, "To connect later, this computer must stay on and awake while the program runs: asleep, it lets nobody in. {HOW}. The screen may go dark");
}

// ── keys ──

/// One line the user typed, without its end; None - the input is over. The app on a phone gives what its user typed or pressed there.
pub fn read_line() -> Option<String> {
    #[cfg(target_os = "android")]
    {
        crate::os::answer()
    }
    #[cfg(not(target_os = "android"))]
    {
        use std::io::BufRead;
        let mut line = String::new();
        match io::stdin().lock().read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line.trim_end_matches(['\r', '\n']).to_string()),
        }
    }
}

/// The key pressed, as it is: a character, or '\n' for Enter; None - the input is over, or Ctrl+C.
pub fn key() -> Option<char> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    let _ = io::stdout().flush();
    crossterm::terminal::enable_raw_mode().ok()?;
    let pressed = loop {
        match event::read() {
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => match k.code {
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => break None,
                KeyCode::Char(c) => break Some(c),
                KeyCode::Enter => break Some('\n'),
                _ => {}
            },
            Ok(_) => {}
            Err(_) => break None,
        }
    };
    let _ = crossterm::terminal::disable_raw_mode();
    pressed
}
