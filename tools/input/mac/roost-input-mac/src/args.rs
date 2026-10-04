//! The command line, parsed with no platform calls so it is tested anywhere.
//!
//! `roost-input-mac [--outdir DIR] [--deadline-ms N] <command> [options]`.
//! Every option takes a value except the flags `--release-all` and
//! `--allow-secure-input`; coordinates are global
//! top-left points (`x,y`, negative on a display left of or above the main
//! one).

use crate::keys::{self, Modifier};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MouseAction {
    Move(Point),
    Down(Point, Button),
    Up(Point, Button),
    Click(Point),
    RightClick(Point),
    CtrlClick(Point),
    /// Press at the first point, drag through the rest, hold at the last for
    /// `hold`, then release there.
    Drag {
        path: Vec<Point>,
        hold: Duration,
        step: Duration,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Preflight {
        pid: Option<i32>,
    },
    Window {
        pid: i32,
    },
    WindowSet {
        pid: i32,
        frame: Rect,
    },
    Key {
        pid: i32,
        codes: Vec<u16>,
        modifiers: Vec<Modifier>,
    },
    ReleaseAll,
    Mouse {
        pid: i32,
        action: MouseAction,
    },
    MenuBar {
        pid: i32,
    },
    Popup {
        pid: i32,
        at: Point,
        wait: Duration,
    },
    Press {
        pid: i32,
        path: Vec<String>,
        at: Option<Point>,
        wait: Duration,
    },
    EventTap {
        duration: Duration,
    },
    Capture {
        rect: Rect,
        out: Option<PathBuf>,
    },
    /// Release exactly what a helper's `held.json` says it still holds.
    ReleaseHeld {
        file: PathBuf,
    },
}

impl Command {
    pub fn name(&self) -> &'static str {
        match self {
            Command::Preflight { .. } => "preflight",
            Command::Window { .. } => "window",
            Command::WindowSet { .. } => "window-set",
            Command::Key { .. } | Command::ReleaseAll => "key",
            Command::Mouse { .. } => "mouse",
            Command::MenuBar { .. } => "menu-bar",
            Command::Popup { .. } => "popup",
            Command::Press { .. } => "press",
            Command::EventTap { .. } => "event-tap",
            Command::Capture { .. } => "capture",
            Command::ReleaseHeld { .. } => "release-held",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Invocation {
    pub outdir: Option<PathBuf>,
    pub deadline: Duration,
    pub command: Command,
    /// `--allow-secure-input`, for `key` and `mouse` only.
    pub allow_secure_input: bool,
}

pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(10);
/// How long a listening tap's process may outlive the listening itself.
const TAP_DEADLINE_MARGIN: Duration = Duration::from_secs(5);
const DEFAULT_WAIT: Duration = Duration::from_millis(2000);
const DEFAULT_DRAG_STEP: Duration = Duration::from_millis(16);

/// `--outdir` alone, read before anything else can fail, so the pid file is
/// written even for an invocation whose remaining arguments are wrong.
pub fn outdir_hint(args: &[String]) -> Option<PathBuf> {
    args.iter()
        .position(|arg| arg == "--outdir")
        .and_then(|index| args.get(index + 1))
        .map(PathBuf::from)
}

struct Options {
    values: Vec<(String, String)>,
    release_all: bool,
}

impl Options {
    fn take(&mut self, name: &str) -> Option<String> {
        let index = self.values.iter().position(|(key, _)| key == name)?;
        Some(self.values.remove(index).1)
    }

    fn require(&mut self, name: &str) -> Result<String, String> {
        self.take(name)
            .ok_or_else(|| format!("--{name} is required"))
    }

    fn millis(&mut self, name: &str) -> Result<Option<Duration>, String> {
        self.take(name)
            .map(|value| {
                value
                    .parse::<u64>()
                    .map(Duration::from_millis)
                    .map_err(|_| format!("--{name} wants whole milliseconds, got `{value}`"))
            })
            .transpose()
    }

    fn finish(self) -> Result<(), String> {
        if self.release_all {
            return Err("--release-all belongs to `key` alone".into());
        }
        match self.values.first() {
            Some((name, _)) => Err(format!("unexpected option --{name}")),
            None => Ok(()),
        }
    }
}

pub fn parse(args: &[String]) -> Result<Invocation, String> {
    let mut positionals: Vec<&str> = Vec::new();
    let mut options = Options {
        values: Vec::new(),
        release_all: false,
    };
    let mut allow_secure_input = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if let Some(name) = arg.strip_prefix("--") {
            if name == "release-all" {
                options.release_all = true;
                index += 1;
                continue;
            }
            if name == "allow-secure-input" {
                allow_secure_input = true;
                index += 1;
                continue;
            }
            let value = args
                .get(index + 1)
                .ok_or_else(|| format!("--{name} needs a value"))?;
            if options.values.iter().any(|(key, _)| key == name) {
                return Err(format!("--{name} given twice"));
            }
            options.values.push((name.to_string(), value.clone()));
            index += 2;
        } else {
            positionals.push(arg);
            index += 1;
        }
    }

    let outdir = options.take("outdir").map(PathBuf::from);
    let explicit_deadline = options.millis("deadline-ms")?;
    let (&name, rest) = positionals
        .split_first()
        .ok_or("no command (want preflight, window, window-set, key, mouse, menu-bar, popup, press, event-tap or capture)")?;
    let command = parse_command(name, rest, &mut options)?;
    options.finish()?;
    if allow_secure_input && !matches!(command, Command::Key { .. } | Command::Mouse { .. }) {
        return Err("--allow-secure-input belongs to `key` and `mouse`".into());
    }

    let deadline = match (&command, explicit_deadline) {
        (Command::EventTap { duration }, None) => *duration + TAP_DEADLINE_MARGIN,
        (Command::EventTap { duration }, Some(deadline)) if deadline <= *duration => {
            return Err("--deadline-ms must outlast the tap's --seconds".into())
        }
        (_, explicit) => explicit.unwrap_or(DEFAULT_DEADLINE),
    };
    if let Command::Popup { wait, .. } | Command::Press { wait, .. } = &command {
        if *wait >= deadline {
            return Err("--wait-ms must be shorter than the deadline".into());
        }
    }
    Ok(Invocation {
        outdir,
        deadline,
        command,
        allow_secure_input,
    })
}

fn parse_command(name: &str, rest: &[&str], options: &mut Options) -> Result<Command, String> {
    let positional_free = |rest: &[&str]| match rest.first() {
        Some(extra) => Err(format!("unexpected argument `{extra}`")),
        None => Ok(()),
    };
    let command = match name {
        "preflight" => Command::Preflight {
            pid: options
                .take("pid")
                .map(|value| parse_pid(&value))
                .transpose()?,
        },
        "window" => Command::Window { pid: pid(options)? },
        "window-set" => Command::WindowSet {
            pid: pid(options)?,
            frame: parse_rect("frame", &options.require("frame")?)?,
        },
        "key" if options.release_all => {
            options.release_all = false;
            Command::ReleaseAll
        }
        "key" => Command::Key {
            pid: pid(options)?,
            codes: parse_codes(&options.require("code")?)?,
            modifiers: keys::parse_modifiers(&options.take("flags").unwrap_or_default())?,
        },
        "mouse" => {
            let (&action, rest) = rest.split_first().ok_or(
                "mouse needs an action: move, down, up, click, right-click, ctrl-click or drag",
            )?;
            positional_free(rest)?;
            return Ok(Command::Mouse {
                pid: pid(options)?,
                action: parse_mouse(action, options)?,
            });
        }
        "menu-bar" => Command::MenuBar { pid: pid(options)? },
        "popup" => Command::Popup {
            pid: pid(options)?,
            at: parse_point("at", &options.require("at")?)?,
            wait: wait(options)?,
        },
        "press" => {
            let path = parse_path(&options.require("path")?)?;
            let at = options
                .take("at")
                .map(|value| parse_point("at", &value))
                .transpose()?;
            if (path[0] == "popup") != at.is_some() {
                return Err("--at goes with a popup path, and only with one".into());
            }
            Command::Press {
                pid: pid(options)?,
                path,
                at,
                wait: wait(options)?,
            }
        }
        "event-tap" => {
            let seconds = options.require("seconds")?;
            let seconds: f64 = seconds
                .parse()
                .map_err(|_| format!("--seconds wants a number, got `{seconds}`"))?;
            if !(seconds.is_finite() && seconds > 0.0) {
                return Err("--seconds must be positive".into());
            }
            Command::EventTap {
                duration: Duration::from_secs_f64(seconds),
            }
        }
        "capture" => Command::Capture {
            rect: parse_rect("rect", &options.require("rect")?)?,
            out: options.take("out").map(PathBuf::from),
        },
        "release-held" => Command::ReleaseHeld {
            file: PathBuf::from(options.require("file")?),
        },
        other => return Err(format!("unknown command `{other}`")),
    };
    positional_free(rest)?;
    Ok(command)
}

fn parse_mouse(action: &str, options: &mut Options) -> Result<MouseAction, String> {
    let at = |options: &mut Options| parse_point("at", &options.require("at")?);
    Ok(match action {
        "move" => MouseAction::Move(at(options)?),
        "down" | "up" => {
            let point = at(options)?;
            let button = match options.take("button").as_deref() {
                None | Some("left") => Button::Left,
                Some("right") => Button::Right,
                Some(other) => return Err(format!("--button wants left or right, got `{other}`")),
            };
            if action == "down" {
                MouseAction::Down(point, button)
            } else {
                MouseAction::Up(point, button)
            }
        }
        "click" => MouseAction::Click(at(options)?),
        "right-click" => MouseAction::RightClick(at(options)?),
        "ctrl-click" => MouseAction::CtrlClick(at(options)?),
        "drag" => {
            let path: Vec<Point> = options
                .require("path")?
                .split(';')
                .map(|point| parse_point("path", point))
                .collect::<Result<_, _>>()?;
            if path.len() < 2 {
                return Err("--path needs at least two points: the press and an end".into());
            }
            MouseAction::Drag {
                path,
                hold: options.millis("hold-ms")?.unwrap_or_default(),
                step: options.millis("step-ms")?.unwrap_or(DEFAULT_DRAG_STEP),
            }
        }
        other => return Err(format!("unknown mouse action `{other}`")),
    })
}

fn pid(options: &mut Options) -> Result<i32, String> {
    parse_pid(&options.require("pid")?)
}

fn wait(options: &mut Options) -> Result<Duration, String> {
    Ok(options.millis("wait-ms")?.unwrap_or(DEFAULT_WAIT))
}

fn parse_pid(value: &str) -> Result<i32, String> {
    match value.parse::<i32>() {
        Ok(pid) if pid > 0 => Ok(pid),
        _ => Err(format!("--pid wants a positive process id, got `{value}`")),
    }
}

fn parse_codes(value: &str) -> Result<Vec<u16>, String> {
    let codes: Vec<u16> = value
        .split(',')
        .map(|code| {
            code.trim()
                .parse::<u16>()
                .ok()
                .filter(|code| *code < 128)
                .ok_or_else(|| format!("--code wants virtual keycodes below 128, got `{code}`"))
        })
        .collect::<Result<_, _>>()?;
    if codes.is_empty() {
        return Err("--code is empty".into());
    }
    Ok(codes)
}

fn numbers<const N: usize>(name: &str, value: &str) -> Result<[f64; N], String> {
    let wrong = || format!("--{name} wants {N} comma-separated numbers, got `{value}`");
    let parts: Vec<f64> = value
        .split(',')
        .map(|part| part.trim().parse::<f64>().ok().filter(|n| n.is_finite()))
        .collect::<Option<_>>()
        .ok_or_else(wrong)?;
    parts.try_into().map_err(|_| wrong())
}

pub fn parse_point(name: &str, value: &str) -> Result<Point, String> {
    let [x, y] = numbers::<2>(name, value)?;
    Ok(Point { x, y })
}

pub fn parse_rect(name: &str, value: &str) -> Result<Rect, String> {
    let [x, y, width, height] = numbers::<4>(name, value)?;
    if width <= 0.0 || height <= 0.0 {
        return Err(format!("--{name} needs a positive width and height"));
    }
    Ok(Rect {
        x,
        y,
        width,
        height,
    })
}

/// A press path is a JSON array, so a title holding `/` or `>` is still one
/// segment: `["menu-bar","View","Enter Full Screen"]`, `["popup","Rename
/// Tab"]` or `["window","AXFullScreenButton"]`. `#N` picks the Nth child.
fn parse_path(value: &str) -> Result<Vec<String>, String> {
    let path: Vec<String> = serde_json::from_str(value)
        .map_err(|error| format!("--path wants a JSON array of strings: {error}"))?;
    match path.first().map(String::as_str) {
        Some("menu-bar" | "popup" | "window") if path.len() >= 2 => Ok(path),
        Some("menu-bar" | "popup" | "window") => Err("--path names nothing to press".into()),
        _ => Err("--path starts with menu-bar, popup or window".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn globals_go_anywhere_and_the_deadline_defaults() {
        let parsed = parse(&args("window --outdir /tmp/o --pid 42")).unwrap();
        assert_eq!(parsed.outdir, Some(PathBuf::from("/tmp/o")));
        assert_eq!(parsed.deadline, DEFAULT_DEADLINE);
        assert_eq!(parsed.command, Command::Window { pid: 42 });
        let parsed = parse(&args("--deadline-ms 2500 preflight")).unwrap();
        assert_eq!(parsed.deadline, Duration::from_millis(2500));
        assert_eq!(parsed.command, Command::Preflight { pid: None });
    }

    #[test]
    fn the_pid_file_location_is_known_even_when_the_rest_is_wrong() {
        let raw = args("--outdir /tmp/o bogus --pid x");
        assert!(parse(&raw).is_err());
        assert_eq!(outdir_hint(&raw), Some(PathBuf::from("/tmp/o")));
    }

    #[test]
    fn key_takes_a_code_list_and_sided_flags() {
        let parsed = parse(&args("key --pid 7 --code 11,36 --flags alt-right")).unwrap();
        let Command::Key {
            pid,
            codes,
            modifiers,
        } = parsed.command
        else {
            panic!("not a key command");
        };
        assert_eq!((pid, codes), (7, vec![11, 36]));
        assert_eq!(modifiers, vec![keys::modifier("alt-right").unwrap()]);
        assert!(parse(&args("key --pid 7 --code 11 --flags alt")).is_err());
        assert!(parse(&args("key --pid 7 --code 200")).is_err());
        assert!(parse(&args("key --code 11")).is_err());
    }

    #[test]
    fn release_held_names_its_journal_and_secure_input_is_opt_in() {
        let parsed = parse(&args("release-held --file /tmp/o/held.json")).unwrap();
        assert_eq!(
            parsed.command,
            Command::ReleaseHeld {
                file: PathBuf::from("/tmp/o/held.json")
            }
        );
        assert!(!parsed.allow_secure_input);
        assert!(parse(&args("release-held")).is_err());
        assert!(
            parse(&args("key --pid 3 --code 0 --allow-secure-input"))
                .unwrap()
                .allow_secure_input
        );
        assert!(parse(&args("mouse click --pid 3 --at 1,1 --allow-secure-input")).is_ok());
        assert!(parse(&args("window --pid 3 --allow-secure-input")).is_err());
    }

    #[test]
    fn release_all_stands_alone() {
        assert_eq!(
            parse(&args("key --release-all")).unwrap().command,
            Command::ReleaseAll
        );
        assert!(parse(&args("window --pid 3 --release-all")).is_err());
    }

    #[test]
    fn negative_global_points_parse() {
        let parsed = parse(&args("mouse click --pid 9 --at -1440.5,-20")).unwrap();
        assert_eq!(
            parsed.command,
            Command::Mouse {
                pid: 9,
                action: MouseAction::Click(Point {
                    x: -1440.5,
                    y: -20.0
                }),
            }
        );
    }

    #[test]
    fn a_drag_needs_two_points_and_keeps_its_timing() {
        let parsed = parse(&args(
            "mouse drag --pid 9 --path 10,20;10,-30;12,-40 --hold-ms 1000",
        ))
        .unwrap();
        let Command::Mouse {
            action: MouseAction::Drag { path, hold, step },
            ..
        } = parsed.command
        else {
            panic!("not a drag");
        };
        assert_eq!(path.len(), 3);
        assert_eq!(path[2], Point { x: 12.0, y: -40.0 });
        assert_eq!(
            (hold, step),
            (Duration::from_millis(1000), DEFAULT_DRAG_STEP)
        );
        assert!(parse(&args("mouse drag --pid 9 --path 10,20")).is_err());
        assert!(parse(&args("mouse down --pid 9 --at 1,2 --button middle")).is_err());
    }

    #[test]
    fn a_press_path_is_a_json_array_with_a_known_root() {
        let raw = vec![
            "press".to_string(),
            "--pid".to_string(),
            "5".to_string(),
            "--path".to_string(),
            r#"["menu-bar","View","Show/Hide Sidebar"]"#.to_string(),
        ];
        let Command::Press { path, at, .. } = parse(&raw).unwrap().command else {
            panic!("not a press");
        };
        assert_eq!(path, ["menu-bar", "View", "Show/Hide Sidebar"]);
        assert_eq!(at, None);
        let popup_without_at = vec![
            "press".to_string(),
            "--pid".to_string(),
            "5".to_string(),
            "--path".to_string(),
            r#"["popup","Rename Tab"]"#.to_string(),
        ];
        assert!(parse(&popup_without_at).is_err());
        assert!(parse_path(r#"["dock","x"]"#).is_err());
        assert!(parse_path(r#"["window"]"#).is_err());
    }

    #[test]
    fn a_tap_outlives_its_listening_and_a_wait_fits_its_deadline() {
        let parsed = parse(&args("event-tap --seconds 3")).unwrap();
        assert_eq!(
            parsed.deadline,
            Duration::from_secs(3) + TAP_DEADLINE_MARGIN
        );
        assert!(parse(&args("event-tap --seconds 3 --deadline-ms 3000")).is_err());
        assert!(parse(&args("event-tap --seconds 0")).is_err());
        assert!(parse(&args(
            "popup --pid 2 --at 1,1 --wait-ms 9000 --deadline-ms 5000"
        ))
        .is_err());
    }

    #[test]
    fn rects_need_four_positive_sized_numbers() {
        assert_eq!(
            parse_rect("frame", "-10,20,300,200").unwrap(),
            Rect {
                x: -10.0,
                y: 20.0,
                width: 300.0,
                height: 200.0
            }
        );
        assert!(parse_rect("frame", "1,2,3").is_err());
        assert!(parse_rect("frame", "1,2,0,4").is_err());
        assert!(parse_point("at", "1,nan").is_err());
    }

    #[test]
    fn strays_and_repeats_are_refused() {
        assert!(parse(&args("window --pid 3 --pid 4")).is_err());
        assert!(parse(&args("window --pid 3 --frame 1,2,3,4")).is_err());
        assert!(parse(&args("window extra --pid 3")).is_err());
        assert!(parse(&args("")).is_err());
        assert!(parse(&args("window --pid 0")).is_err());
    }
}
