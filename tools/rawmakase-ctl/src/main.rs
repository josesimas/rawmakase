//! `rawmakase-ctl`: does from a script what the Loupedeck does by hand.
//!
//! RAWmakase listens on a loopback socket and writes where to `control.json`
//! in its data folder (see src/app/control_surface/socket.rs). Each command
//! here is one request; the app answers once a frame has handled it.
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    net::{Ipv4Addr, TcpStream},
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};

/// `println!` that ends quietly when the reader has gone, as with `| head`.
macro_rules! out {
    ($($arg:tt)*) => {
        if writeln!(std::io::stdout(), $($arg)*).is_err() {
            std::process::exit(0);
        }
    };
}

/// The device's dials and buttons, as mapped with probe.py.
const CONTROLS: &str = include_str!("../../loupedeck/controls.json");

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// RAWmakase's data folder, where it leaves control.json
    /// [default: $RAWMAKASE_DATA_DIR, else the app's own default]
    #[arg(long, global = true, value_name = "DIR")]
    data_dir: Option<PathBuf>,
    /// Print the app's state after the command, as JSON
    #[arg(long, short, global = true)]
    state: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the app's state: the mode, the open photo and every slider
    State,
    /// Print one slider's value
    Get {
        /// exposure, temperature, tint, contrast, highlights, shadows, whites,
        /// blacks, texture, clarity, dehaze, vibrance, saturation, or
        /// band1..band8 with .hue .sat .lum .gray
        slider: String,
    },
    /// Set a slider: EV for exposure, kelvin for temperature, else -100..100
    Set { slider: String, value: f64 },
    /// Turn a slider by ticks (clockwise is positive), as a dial would
    Turn { slider: String, ticks: i32 },
    /// Turn a Loupedeck dial by name (Exposure, "Fader P3", control_dial) or CC number
    Dial { control: String, ticks: i32 },
    /// Press a Loupedeck button by name (P7, Undo, shift) or note number
    Press {
        button: String,
        /// Only press it, as to hold a modifier
        #[arg(long, conflicts_with = "up")]
        down: bool,
        /// Only release it
        #[arg(long)]
        up: bool,
    },
    /// Press a key as the keyboard would: "cmd+shift+z", "3", "p", "left"
    Key { combo: String },
    /// Show Hue, Sat or Lum in the Color Mixer, which the band faders then turn
    Mixer { channel: String },
    /// Turn Black & White on or off
    Bw,
    /// Go to the next or previous photo in Develop and the Loupe
    Photo { direction: Direction },
    /// List the Loupedeck's dial and button names
    Controls,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Direction {
    Next,
    Prev,
}

#[derive(Debug, PartialEq)]
struct Control {
    id: String,
    label: String,
    number: u8,
}

/// The dials (`cc`) or buttons (`note`) in controls.json.
fn controls(section: &str, key: &str) -> Vec<Control> {
    let all: Value = serde_json::from_str(CONTROLS).expect("controls.json is valid");
    all[section]["controls"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            Some(Control {
                id: c["id"].as_str()?.into(),
                label: c["label"].as_str()?.into(),
                number: u8::try_from(c[key].as_u64()?).ok()?,
            })
        })
        .collect()
}

/// "Fader P3", "fader_p3" and "FADER-P3" are one name.
fn squash(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// A control by number, id or label.
fn find(list: &[Control], name: &str) -> Result<u8, String> {
    if let Ok(number) = name.parse::<u8>() {
        return (number < 128)
            .then_some(number)
            .ok_or_else(|| format!("{number} is not a MIDI number (0 to 127)"));
    }
    let wanted = squash(name);
    list.iter()
        .find(|c| squash(&c.id) == wanted || squash(&c.label) == wanted)
        .map(|c| c.number)
        .ok_or_else(|| format!("no control called \"{name}\" (see `rawmakase-ctl controls`)"))
}

/// The CC values for a dial turned by `ticks`: 1..=63 clockwise, 128 - n counter-clockwise.
fn dial_values(ticks: i32) -> Vec<u8> {
    let mut left = ticks;
    let mut values = Vec::new();
    while left != 0 {
        let n = left.clamp(-63, 63);
        values.push(if n > 0 { n as u8 } else { (128 + n) as u8 });
        left -= n;
    }
    values
}

/// Where RAWmakase keeps its data, as src/storage/files.rs works it out.
fn default_data_dir() -> PathBuf {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
    let home = || {
        PathBuf::from(
            var("HOME")
                .or_else(|| var("USERPROFILE"))
                .unwrap_or_default(),
        )
    };
    if let Some(dir) = var("RAWMAKASE_DATA_DIR") {
        PathBuf::from(dir)
    } else if cfg!(windows)
        && let Some(appdata) = var("APPDATA")
    {
        PathBuf::from(appdata).join("RAWmakase")
    } else if cfg!(target_os = "macos") {
        home().join("Library/Application Support/RAWmakase")
    } else if cfg!(windows) {
        home().join(".local/share/rawmakase")
    } else {
        var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home().join(".local/share"))
            .join("rawmakase")
    }
}

struct Connection {
    port: u16,
    token: String,
}
impl Connection {
    fn read(dir: &std::path::Path) -> Result<Self, String> {
        let path = dir.join("control.json");
        let text = std::fs::read_to_string(&path).map_err(|e| {
            format!(
                "cannot read {}: {e}\nIs RAWmakase (the Loupedeck fork) running? Its data folder \
                 is set by RAWMAKASE_DATA_DIR or --data-dir.",
                path.display()
            )
        })?;
        let json: Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self {
            port: json["port"]
                .as_u64()
                .and_then(|p| u16::try_from(p).ok())
                .ok_or("control.json has no port")?,
            token: json["token"]
                .as_str()
                .ok_or("control.json has no token")?
                .into(),
        })
    }
    /// Sends a request and returns the app's state after it.
    fn ask(&self, mut request: Value) -> Result<Value, String> {
        request["token"] = self.token.clone().into();
        let mut stream = TcpStream::connect_timeout(
            &(Ipv4Addr::LOCALHOST, self.port).into(),
            Duration::from_secs(2),
        )
        .map_err(|e| {
            format!(
                "cannot reach RAWmakase on port {}: {e} (control.json may be stale)",
                self.port
            )
        })?;
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        writeln!(stream, "{request}").map_err(|e| e.to_string())?;
        let mut line = String::new();
        BufReader::new(stream)
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        let reply: Value = serde_json::from_str(&line).map_err(|e| format!("bad reply: {e}"))?;
        if reply["ok"] == true {
            Ok(reply["state"].clone())
        } else {
            Err(reply["error"].as_str().unwrap_or("refused").into())
        }
    }
}

/// What the command asks of the app: its requests, in order.
fn requests(command: &Command) -> Result<Vec<Value>, String> {
    Ok(match command {
        Command::State | Command::Get { .. } | Command::Controls => vec![json!({"cmd": "state"})],
        Command::Set { slider, value } => {
            vec![json!({"cmd": "set", "param": slider, "value": value})]
        }
        Command::Turn { slider, ticks } => {
            vec![json!({"cmd": "turn", "param": slider, "ticks": ticks})]
        }
        Command::Dial { control, ticks } => {
            let cc = find(&controls("dials", "cc"), control)?;
            dial_values(*ticks)
                .into_iter()
                .map(|value| json!({"cmd": "cc", "cc": cc, "value": value}))
                .collect()
        }
        Command::Press { button, down, up } => {
            let note = find(&controls("buttons", "note"), button)?;
            let press = if *down {
                "down"
            } else if *up {
                "up"
            } else {
                "click"
            };
            vec![json!({"cmd": "note", "note": note, "press": press})]
        }
        Command::Key { combo } => vec![json!({"cmd": "action", "action": combo})],
        Command::Mixer { channel } => {
            vec![json!({"cmd": "action", "action": format!("mixer:{channel}")})]
        }
        Command::Bw => vec![json!({"cmd": "action", "action": "toggle:bw"})],
        Command::Photo { direction } => {
            let step = match direction {
                Direction::Next => 1,
                Direction::Prev => -1,
            };
            vec![json!({"cmd": "photo", "step": step})]
        }
    })
}

fn run(cli: Cli) -> Result<(), String> {
    if let Command::Controls = cli.command {
        for (title, section, key) in [
            ("Dials (CC)", "dials", "cc"),
            ("Buttons (note)", "buttons", "note"),
        ] {
            out!("{title}");
            for c in controls(section, key) {
                out!("  {:>3}  {:<14} {}", c.number, c.id, c.label);
            }
        }
        return Ok(());
    }
    let connection = Connection::read(&cli.data_dir.clone().unwrap_or_else(default_data_dir))?;
    let mut state = Value::Null;
    for request in requests(&cli.command)? {
        state = connection.ask(request)?;
    }
    match &cli.command {
        Command::State => out!("{}", serde_json::to_string_pretty(&state).unwrap()),
        Command::Get { slider } => {
            let key = slider.to_ascii_lowercase();
            match state["values"]
                .get(&key)
                .or_else(|| state["values"].get(format!("{key}.hue")))
            {
                Some(value) => out!("{value}"),
                None if state["mode"] != "develop" || state["photo"].is_null() => {
                    return Err("no photo is open in Develop".into());
                }
                None => return Err(format!("unknown slider \"{slider}\"")),
            }
        }
        _ if cli.state => out!("{}", serde_json::to_string_pretty(&state).unwrap()),
        _ => {}
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rawmakase-ctl: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_are_found_by_id_label_or_number() {
        let dials = controls("dials", "cc");
        assert_eq!(dials.len(), 22);
        assert_eq!(find(&dials, "Exposure"), Ok(33));
        assert_eq!(find(&dials, "Fader P3"), Ok(19));
        assert_eq!(find(&dials, "fader_p3"), Ok(19));
        assert_eq!(find(&dials, "control-dial"), Ok(48));
        assert_eq!(find(&dials, "7"), Ok(7));
        assert!(find(&dials, "200").is_err());
        assert!(find(&dials, "nonsense").unwrap_err().contains("no control"));
        let buttons = controls("buttons", "note");
        assert_eq!(buttons.len(), 40);
        assert_eq!(find(&buttons, "P7"), Ok(86));
        assert_eq!(find(&buttons, "Undo"), Ok(95));
    }

    #[test]
    fn dial_turns_are_relative_cc_values() {
        assert_eq!(dial_values(1), [1]);
        assert_eq!(dial_values(-1), [127]);
        assert_eq!(dial_values(-5), [123]);
        assert_eq!(dial_values(70), [63, 7]);
        assert_eq!(dial_values(-64), [65, 127]);
        assert!(dial_values(0).is_empty());
    }

    #[test]
    fn commands_become_requests() {
        let cli = Cli::try_parse_from(["rawmakase-ctl", "dial", "exposure", "--", "-70"]).unwrap();
        let sent = requests(&cli.command).unwrap();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0], json!({"cmd": "cc", "cc": 33, "value": 65}));
        let cli = Cli::try_parse_from(["rawmakase-ctl", "press", "shift", "--down"]).unwrap();
        assert_eq!(
            requests(&cli.command).unwrap(),
            [json!({"cmd": "note", "note": 66, "press": "down"})]
        );
        let cli = Cli::try_parse_from(["rawmakase-ctl", "set", "exposure", "0.5"]).unwrap();
        assert_eq!(
            requests(&cli.command).unwrap(),
            [json!({"cmd": "set", "param": "exposure", "value": 0.5})]
        );
    }
}
