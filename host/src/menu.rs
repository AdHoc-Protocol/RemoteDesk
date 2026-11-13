// The start of the program by a double click: no arguments, a window of its own. The user of the machine is asked how the viewer
// will reach it - a number to press, the key alone, never an option to type - and the answer becomes the arguments a start from a
// console takes, so all that follows is the same. The answer is kept beside the program ( rdp-host.settings ): the next start offers
// it on Enter.
//
// Typed is only what cannot be chosen: the meeting point and its password, the first time. Started with arguments, or without a
// console to ask in ( a service, a script ), the program does as it is told and asks nothing.

use crate::console::paint;
use crossterm::style::Color;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::thread::{self, ThreadId};

/// How the viewer reaches the machine: the ways of the README.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Way {
    /// ① in this network
    Here,
    /// ② from the internet: the machine or its router has a public address
    Internet,
    /// ③ through a meeting point of SYN Mirror
    Meet,
    /// ④ straight, nothing set up
    Direct,
}

/// The way chosen in the menu; None - started with arguments.
static CHOSEN: OnceLock<Way> = OnceLock::new();

/// The thread the menu asked on: a failure there leaves the window open until Enter, so the user reads why.
static ASKED_ON: OnceLock<ThreadId> = OnceLock::new();

pub fn chosen() -> Option<Way> { CHOSEN.get().copied() }

/// The arguments of the program: as given; or, started by a double click, what the menu makes of the answer.
pub fn arguments() -> Vec<String> {
    crate::console::banner();
    let given: Vec<String> = std::env::args().skip(1).collect();
    if !given.is_empty() || !io::stdin().is_terminal() {
        return given;
    }
    let _ = ASKED_ON.set(thread::current().id());
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        if ASKED_ON.get() == Some(&thread::current().id()) {
            wait_to_close();
        }
    }));
    let (way, args) = ask(&mut io::stdin().lock(), true, &Kept::file());
    let _ = CHOSEN.set(way);
    args
}

/// What the user of a window opened by a double click needs before it closes: the time to read it.
pub fn wait_to_close() {
    if CHOSEN.get().is_none() {
        return;
    }
    print!("\n {}", paint("Press Enter to close this window.", Color::Cyan, false));
    let _ = io::stdout().flush();
    let _ = io::stdin().lock().read_line(&mut String::new());
    std::process::exit(1);
}

/// The ways as the menu shows them: the sign, the name, a word on when.
const WAYS: [(&str, &str, &str); 4] = [
    ("🏠", "From this network", "the same home or office"),
    ("🌍", "From the internet", "this computer or its router has a public address"),
    ("🤝", "Meeting point", "SYN Mirror on a router of yours"),
    ("⚡", "Directly", "nothing to set up"),
];

/// The questions, answered from `input` - the keys of the console, when `keys`; what is kept, in `file`. The way chosen, and the
/// arguments it makes.
fn ask(input: &mut dyn BufRead, keys: bool, file: &std::path::Path) -> (Way, Vec<String>) {
    let mut kept = Kept::load(file);
    say!("🧭", News, "How will the viewer reach this computer?");
    println!();
    for (i, (sign, name, when)) in WAYS.iter().enumerate() {
        let when = match (i, &kept.meet) {
            (2, Some(m)) => format!("SYN Mirror: {m}"),
            _ => when.to_string(),
        };
        println!("    {}  {sign}  {}{}", paint(i + 1, Color::Yellow, true), paint(format!("{name:<20}"), Color::White, true), paint(when, Color::DarkGrey, false));
    }
    println!();
    let last = kept.way;
    print!(" {} ", match last {
        Some(w) => paint(format!("Press 1, 2, 3 or 4 · Enter: {} as last time ›", w as u8 + 1), Color::Cyan, false),
        None => paint("Press 1, 2, 3 or 4 ›", Color::Cyan, false),
    });
    // the key is the answer: nothing to press after it
    let way = loop {
        match press(input, keys) {
            None => std::process::exit(0),
            Some('1') => break Way::Here,
            Some('2') => break Way::Internet,
            Some('3') => break Way::Meet,
            Some('4') => break Way::Direct,
            Some('\n') if last.is_some() => break last.unwrap(),
            _ => {}
        }
    };
    let (sign, name, _) = WAYS[way as usize];
    println!("{}", paint(format!("{}  {sign} {name}", way as u8 + 1), Color::Green, true));
    let mut line = |prompt: &str| line(input, prompt);

    let mut args = Vec::new();
    match way {
        Way::Here | Way::Internet => {}
        Way::Direct => args.push(String::from("--direct")),
        Way::Meet => {
            // the point and its password as its owner gave them: kept, offered on Enter
            let point = loop {
                let typed = match &kept.meet {
                    Some(m) => line(&format!("The address of the meeting point - Enter for {m}: ")),
                    None => line("The address of the meeting point, as its owner gave it: "),
                };
                let point = if typed.is_empty() { kept.meet.clone().unwrap_or_default() } else { typed };
                if crate::meet::Meet::point(&point).is_some() {
                    break point;
                }
                say!("💡", Warn, "A name or an address, and :port when its owner gave one - such as router.example.org:40000");
            };
            let same_point = kept.meet.as_deref() == Some(point.as_str());
            let password = loop {
                let typed = match (&kept.meet_password, same_point) {
                    (Some(_), true) => line("Its password, 8 characters - Enter for the same as last time: "),
                    _ => line("Its password, 8 characters: "),
                };
                let password = if typed.is_empty() && same_point { kept.meet_password.clone().unwrap_or_default() } else { typed };
                if password.len() == 8 {
                    break password;
                }
                say!("💡", Warn, "The password of a meeting point is 8 characters");
            };
            args.extend([String::from("--meet"), point.clone(), String::from("--meet-password"), password.clone()]);
            kept.meet = Some(point);
            kept.meet_password = Some(password);
        }
    }
    kept.way = Some(way);
    kept.save(file);
    println!();
    (way, args)
}

/// The key pressed: of the console, when `keys`; else the next character of `input` ( '\n' - Enter ). None - the input is over.
fn press(input: &mut dyn BufRead, keys: bool) -> Option<char> {
    if keys {
        return crate::console::key();
    }
    let mut b = [0u8];
    loop {
        if input.read(&mut b).ok()? == 0 {
            return None;
        }
        if b[0] != b'\r' {
            return Some(b[0] as char);
        }
    }
}

/// A line the user typed, trimmed; the end of the input - the window is closing.
fn line(input: &mut dyn BufRead, prompt: &str) -> String {
    print!(" {} ", paint(prompt, Color::Cyan, false));
    let _ = io::stdout().flush();
    let mut s = String::new();
    if input.read_line(&mut s).unwrap_or(0) == 0 {
        std::process::exit(0);
    }
    s.trim().to_string()
}

/// What the menu keeps beside the program: the last way, the meeting point and its password. Lines `key = value`.
struct Kept {
    way:           Option<Way>,
    meet:          Option<String>,
    meet_password: Option<String>,
}

impl Kept {
    fn file() -> PathBuf {
        std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("rdp-host.settings"))).unwrap_or_else(|| PathBuf::from("rdp-host.settings"))
    }

    fn load(file: &std::path::Path) -> Self {
        let mut kept = Kept { way: None, meet: None, meet_password: None };
        for l in std::fs::read_to_string(file).unwrap_or_default().lines() {
            let Some((k, v)) = l.split_once('=') else { continue };
            let v = v.trim().to_string();
            match k.trim() {
                "way" => kept.way = [Way::Here, Way::Internet, Way::Meet, Way::Direct].get(v.parse::<usize>().unwrap_or(0).wrapping_sub(1)).copied(),
                "meet" if !v.is_empty() => kept.meet = Some(v),
                "meet_password" if !v.is_empty() => kept.meet_password = Some(v),
                _ => {}
            }
        }
        kept
    }

    fn save(&self, file: &std::path::Path) {
        let mut s = String::from("# RemoteDesk: what the window of the program offers on Enter\n");
        if let Some(w) = self.way {
            s += &format!("way = {}\n", w as u8 + 1);
        }
        if let Some(m) = &self.meet {
            s += &format!("meet = {m}\n");
        }
        if let Some(p) = &self.meet_password {
            s += &format!("meet_password = {p}\n");
        }
        if let Err(e) = std::fs::write(file, s) {
            say!("💾", Quiet, "Not kept for the next start: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(answers: &str, file: &std::path::Path) -> (Way, Vec<String>) { ask(&mut io::Cursor::new(answers.as_bytes().to_vec()), false, file) }

    #[test]
    fn the_menu_asks_and_keeps() {
        let file = std::env::temp_dir().join(format!("rdp-host-menu-{}.settings", std::process::id()));
        let _ = std::fs::remove_file(&file);

        // the first start: Enter is no answer yet, 7 is none; the key 4 - directly, nothing after it
        assert_eq!(run("\n74", &file), (Way::Direct, vec![String::from("--direct")]));
        // the next one: Enter - as last time
        assert_eq!(run("\n", &file), (Way::Direct, vec![String::from("--direct")]));
        // a meeting point: a wrong one, then the right one; a short password, then the right one
        assert_eq!(
            run("3router:abc\nrouter.example.org:40000\nshort\nTr7kQ2xZ\n", &file),
            (Way::Meet, ["--meet", "router.example.org:40000", "--meet-password", "Tr7kQ2xZ"].map(String::from).to_vec())
        );
        // kept: Enter, Enter, Enter - the same way, point and password
        assert_eq!(run("\n\n\n", &file).1, ["--meet", "router.example.org:40000", "--meet-password", "Tr7kQ2xZ"].map(String::from).to_vec());
        // another point: its password is asked anew
        assert_eq!(run("3other.example.org:41000\n\nAb12Cd34\n", &file).1, ["--meet", "other.example.org:41000", "--meet-password", "Ab12Cd34"].map(String::from).to_vec());
        // this network, and the internet: no arguments, the way tells the lines
        assert_eq!(run("1", &file), (Way::Here, vec![]));
        assert_eq!(run("2", &file), (Way::Internet, vec![]));
        assert!(std::fs::read_to_string(&file).unwrap().contains("way = 2"));
        let _ = std::fs::remove_file(&file);
    }
}
