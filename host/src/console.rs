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

/// The line for whoever connects, in a frame, and in the clipboard - said so: pasting it into a message is all that is left.
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
    println!();
}

// ── keys ──

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
