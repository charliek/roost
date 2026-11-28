//! What the app keeps in its data directory: the saved hosts
//! (`hosts.json`), this install's `client_id` and the display settings
//! (`settings.json`). Every write is a temporary file renamed over the old
//! one, so a crash mid-write leaves the previous file whole.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use roost_ssh::{HostTarget, TargetError};
use roost_ui_model::theme::Theme;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const HOSTS_FILE: &str = "hosts.json";
const CLIENT_ID_FILE: &str = "client_id";
const SETTINGS_FILE: &str = "settings.json";
const SETTINGS_BAD_FILE: &str = "settings.json.bad";
const HOSTS_VERSION: u32 = 1;
const SETTINGS_VERSION: u32 = 1;

/// One saved host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostRecord {
    pub id: String,
    pub label: String,
    /// `user@host[:port]` over Tailscale SSH, or `unix:/path/to/roost.sock`.
    pub target: String,
    /// The `SHA256:…` fingerprint the user trusted.
    #[serde(default)]
    pub pinned_host_key: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct HostsFile {
    version: u32,
    hosts: Vec<HostRecord>,
}

/// Where a target string says to dial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Unix(PathBuf),
    Ssh(HostTarget),
}

impl Target {
    /// A phone cannot know a host's login user, so an SSH target must
    /// name one.
    pub fn parse(target: &str) -> Result<Self, String> {
        if let Some(path) = target.strip_prefix("unix:") {
            if path.is_empty() {
                return Err("a unix: target needs a socket path".into());
            }
            return Ok(Target::Unix(PathBuf::from(path)));
        }
        match HostTarget::parse(target, "") {
            Ok(target) => Ok(Target::Ssh(target)),
            Err(TargetError::MissingUser(_)) => Err(format!(
                "{target:?} names no user: write it as user@host, with the user Tailscale SSH \
                 logs in as"
            )),
            Err(error) => Err(error.to_string()),
        }
    }
}

pub fn load_hosts(dir: &Path) -> std::io::Result<Vec<HostRecord>> {
    let bytes = match std::fs::read(dir.join(HOSTS_FILE)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let file: HostsFile = serde_json::from_slice(&bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    Ok(file.hosts)
}

pub fn save_hosts(dir: &Path, hosts: &[HostRecord]) -> std::io::Result<()> {
    let file = HostsFile {
        version: HOSTS_VERSION,
        hosts: hosts.to_vec(),
    };
    let mut bytes = serde_json::to_vec_pretty(&file).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    write_atomic(&dir.join(HOSTS_FILE), &bytes)
}

/// Light or dark app chrome and terminal, or whichever the platform says.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Appearance {
    System,
    #[default]
    Dark,
    Light,
}

/// Raise faint and low-contrast text to a minimum contrast: `Auto` does
/// so only on a light terminal theme.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReadableText {
    #[default]
    Auto,
    On,
    Off,
}

/// A string in the file, so a value this build does not know resets only
/// its own field rather than failing the whole file.
macro_rules! string_enum {
    ($ty:ty { $($variant:ident = $name:literal),+ $(,)? }) => {
        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(match self {
                    $(Self::$variant => $name),+
                })
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                Ok(match String::deserialize(deserializer)?.as_str() {
                    $($name => Self::$variant,)+
                    _ => Self::default(),
                })
            }
        }
    };
}

string_enum!(Appearance { System = "system", Dark = "dark", Light = "light" });
string_enum!(ReadableText { Auto = "auto", On = "on", Off = "off" });

/// The phone's display settings (`settings.json`). Every field missing
/// from the file takes its default; [`Self::sanitize`] brings any value
/// into range.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MobileSettings {
    pub version: u32,
    pub terminal_font_size_compact: f64,
    pub terminal_font_size_wide: f64,
    pub ui_text_scale: f64,
    pub appearance: Appearance,
    pub dark_theme: String,
    pub light_theme: String,
    pub readable_text: ReadableText,
    pub bright_faint: bool,
}

pub const DEFAULT_TERMINAL_FONT_SIZE: f64 = 13.0;
pub const DEFAULT_DARK_THEME: &str = "roost-dark";
pub const DEFAULT_LIGHT_THEME: &str = "Catppuccin Latte";

impl Default for MobileSettings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            terminal_font_size_compact: DEFAULT_TERMINAL_FONT_SIZE,
            terminal_font_size_wide: DEFAULT_TERMINAL_FONT_SIZE,
            ui_text_scale: 1.0,
            appearance: Appearance::default(),
            dark_theme: DEFAULT_DARK_THEME.into(),
            light_theme: DEFAULT_LIGHT_THEME.into(),
            readable_text: ReadableText::default(),
            bright_faint: false,
        }
    }
}

const TERMINAL_FONT_RANGE: (f64, f64) = (7.0, 28.0);

impl MobileSettings {
    /// Every field in range: sizes clamped and rounded to their step,
    /// theme names the canonical spelling of a bundled theme of the right
    /// brightness, anything else its default.
    #[must_use]
    pub fn sanitize(self) -> Self {
        let defaults = Self::default();
        Self {
            version: SETTINGS_VERSION,
            terminal_font_size_compact: stepped(
                self.terminal_font_size_compact,
                defaults.terminal_font_size_compact,
                TERMINAL_FONT_RANGE,
                2.0,
            ),
            terminal_font_size_wide: stepped(
                self.terminal_font_size_wide,
                defaults.terminal_font_size_wide,
                TERMINAL_FONT_RANGE,
                2.0,
            ),
            ui_text_scale: stepped(
                self.ui_text_scale,
                defaults.ui_text_scale,
                (0.85, 1.6),
                20.0,
            ),
            appearance: self.appearance,
            dark_theme: canonical_theme(&self.dark_theme, &Theme::bundled_names())
                .unwrap_or(defaults.dark_theme),
            light_theme: canonical_theme(&self.light_theme, &Theme::bundled_light_names())
                .unwrap_or(defaults.light_theme),
            readable_text: self.readable_text,
            bright_faint: self.bright_faint,
        }
    }
}

/// `value` clamped to `range` and rounded to the nearest `1 / per_unit`;
/// `default` when it is not a number.
fn stepped(value: f64, default: f64, (min, max): (f64, f64), per_unit: f64) -> f64 {
    if !value.is_finite() {
        return default;
    }
    (value.clamp(min, max) * per_unit).round() / per_unit
}

fn canonical_theme(name: &str, names: &[String]) -> Option<String> {
    names
        .iter()
        .find(|known| known.eq_ignore_ascii_case(name.trim()))
        .cloned()
}

/// The saved settings, sanitized. A missing file is the defaults; so is a
/// malformed one, which is kept beside it as `settings.json.bad` for a
/// look later. Any other read error is returned.
pub fn load_settings(dir: &Path) -> std::io::Result<MobileSettings> {
    let path = dir.join(SETTINGS_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(MobileSettings::default())
        }
        Err(error) => return Err(error),
    };
    let settings: MobileSettings = match serde_json::from_slice(&bytes) {
        Ok(settings) => settings,
        Err(error) => {
            tracing::warn!(%error, "settings.json is malformed; kept as settings.json.bad, using the defaults");
            std::fs::rename(&path, dir.join(SETTINGS_BAD_FILE))?;
            return Ok(MobileSettings::default());
        }
    };
    if settings.version > SETTINGS_VERSION {
        tracing::warn!(
            version = settings.version,
            "settings.json is from a newer app; reading the fields this one knows"
        );
    }
    Ok(settings.sanitize())
}

pub fn save_settings(dir: &Path, settings: &MobileSettings) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(settings).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    write_atomic(&dir.join(SETTINGS_FILE), &bytes)
}

/// This install's `client_id`, minted on first use: what every session
/// knows this phone by, so it must not change across launches.
pub fn client_id(dir: &Path) -> std::io::Result<String> {
    let path = dir.join(CLIENT_ID_FILE);
    match std::fs::read_to_string(&path) {
        Ok(id) if valid_client_id(id.trim()) => return Ok(id.trim().to_string()),
        Ok(_) => tracing::warn!("client_id file is malformed; minting a new id"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let id = new_id();
    write_atomic(&path, format!("{id}\n").as_bytes())?;
    Ok(id)
}

fn valid_client_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// 16 random lowercase hex characters.
pub fn new_id() -> String {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("the OS random source must be available");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Write through a temporary file of this write's own, then rename it
/// over `path`. Two writes sharing one temporary would truncate and
/// interleave each other's bytes before either rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(
        ".tmp.{}.{}",
        std::process::id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = PathBuf::from(tmp);
    let written = std::fs::File::create(&tmp).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    let renamed = written.and_then(|()| std::fs::rename(&tmp, path));
    if renamed.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    renamed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_round_trip_through_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            load_hosts(dir.path()).expect("load").is_empty(),
            "no file yet"
        );
        let hosts = vec![
            HostRecord {
                id: new_id(),
                label: "popos".into(),
                target: "charlie@popos".into(),
                pinned_host_key: Some("SHA256:abc".into()),
            },
            HostRecord {
                id: new_id(),
                label: "dev".into(),
                target: "unix:/tmp/x.sock".into(),
                pinned_host_key: None,
            },
        ];
        save_hosts(dir.path(), &hosts).expect("save");
        assert_eq!(load_hosts(dir.path()).expect("load"), hosts);
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// Long and short lists saved at once: every load parses, as one of
    /// the lists written whole.
    #[test]
    fn concurrent_saves_never_interleave() {
        let dir = tempfile::tempdir().expect("tempdir");
        let list = |n: usize| -> Vec<HostRecord> {
            (0..n)
                .map(|at| HostRecord {
                    id: format!("{at:016x}"),
                    label: "x".repeat(40),
                    target: format!("user@host-{at}"),
                    pinned_host_key: None,
                })
                .collect()
        };
        std::thread::scope(|scope| {
            for writer in 0..8 {
                let dir = dir.path();
                let hosts = list(if writer % 2 == 0 { 50 } else { 1 });
                scope.spawn(move || {
                    for _ in 0..40 {
                        save_hosts(dir, &hosts).expect("save");
                    }
                });
            }
        });
        let loaded = load_hosts(dir.path()).expect("a whole file");
        assert!(
            loaded == list(50) || loaded == list(1),
            "{} hosts",
            loaded.len()
        );
    }

    #[test]
    fn the_client_id_is_minted_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = client_id(dir.path()).expect("mint");
        assert_eq!(first.len(), 16);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(client_id(dir.path()).expect("reload"), first);

        std::fs::write(dir.path().join(CLIENT_ID_FILE), "not ok!\n").expect("corrupt");
        let replaced = client_id(dir.path()).expect("re-mint");
        assert_ne!(replaced, first);
        assert!(valid_client_id(&replaced));
    }

    fn write_settings(dir: &Path, json: &str) {
        std::fs::write(dir.join(SETTINGS_FILE), json).expect("write settings.json");
    }

    #[test]
    fn settings_round_trip_through_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = MobileSettings {
            version: SETTINGS_VERSION,
            terminal_font_size_compact: 11.5,
            terminal_font_size_wide: 16.0,
            ui_text_scale: 1.25,
            appearance: Appearance::System,
            dark_theme: "Catppuccin Mocha".into(),
            light_theme: "GitHub Light Default".into(),
            readable_text: ReadableText::Off,
            bright_faint: true,
        };
        save_settings(dir.path(), &settings).expect("save");
        assert_eq!(load_settings(dir.path()).expect("load"), settings);
        let saved = std::fs::read_to_string(dir.path().join(SETTINGS_FILE)).expect("read");
        assert!(saved.contains(r#""appearance": "system""#), "{saved}");
        assert!(saved.contains(r#""readable_text": "off""#), "{saved}");
    }

    #[test]
    fn missing_or_empty_settings_are_the_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            load_settings(dir.path()).expect("no file"),
            MobileSettings::default()
        );
        write_settings(dir.path(), "{}");
        let empty = load_settings(dir.path()).expect("{}");
        assert_eq!(empty, MobileSettings::default());
        assert_eq!(empty.terminal_font_size_wide, 13.0, "a default, not zero");
        assert_eq!(empty.ui_text_scale, 1.0, "a default, not zero");
    }

    #[test]
    fn a_partial_older_file_keeps_what_it_has_and_ignores_the_unknown() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_settings(
            dir.path(),
            r#"{"version": 1, "terminal_font_size_compact": 15, "from_the_future": [1, 2]}"#,
        );
        let settings = load_settings(dir.path()).expect("load");
        assert_eq!(
            settings,
            MobileSettings {
                terminal_font_size_compact: 15.0,
                ..MobileSettings::default()
            }
        );
    }

    #[test]
    fn a_newer_version_loads_what_it_can_and_is_written_as_1() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_settings(dir.path(), r#"{"version": 7, "appearance": "light"}"#);
        let settings = load_settings(dir.path()).expect("load");
        assert_eq!(settings.appearance, Appearance::Light);
        save_settings(dir.path(), &settings).expect("save");
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join(SETTINGS_FILE)).expect("read"))
                .expect("json");
        assert_eq!(saved["version"], 1);
        assert_eq!(saved["appearance"], "light");
    }

    #[test]
    fn an_unknown_enum_value_resets_only_its_own_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_settings(
            dir.path(),
            r#"{"appearance": "sepia", "readable_text": "maybe", "terminal_font_size_wide": 18}"#,
        );
        let settings = load_settings(dir.path()).expect("load");
        assert_eq!(settings.appearance, Appearance::Dark);
        assert_eq!(settings.readable_text, ReadableText::Auto);
        assert_eq!(settings.terminal_font_size_wide, 18.0);
    }

    #[test]
    fn sanitize_clamps_rounds_and_resets_non_numbers() {
        let clean = |settings: MobileSettings| settings.sanitize();
        let sizes = |compact: f64, wide: f64, scale: f64| {
            let settings = clean(MobileSettings {
                terminal_font_size_compact: compact,
                terminal_font_size_wide: wide,
                ui_text_scale: scale,
                ..MobileSettings::default()
            });
            (
                settings.terminal_font_size_compact,
                settings.terminal_font_size_wide,
                settings.ui_text_scale,
            )
        };
        assert_eq!(
            sizes(f64::NAN, f64::INFINITY, f64::NEG_INFINITY),
            (13.0, 13.0, 1.0)
        );
        assert_eq!(sizes(2.0, 99.0, 0.1), (7.0, 28.0, 0.85));
        assert_eq!(sizes(13.26, 13.24, 1.234), (13.5, 13.0, 1.25));
        assert_eq!(sizes(-0.0, 28.0, 5.0), (7.0, 28.0, 1.6));
    }

    #[test]
    fn sanitize_canonicalizes_theme_names_and_keeps_each_to_its_brightness() {
        let themes = |dark: &str, light: &str| {
            let settings = MobileSettings {
                dark_theme: dark.into(),
                light_theme: light.into(),
                ..MobileSettings::default()
            }
            .sanitize();
            (settings.dark_theme, settings.light_theme)
        };
        assert_eq!(
            themes("catppuccin mocha", "catppuccin latte"),
            ("Catppuccin Mocha".into(), "Catppuccin Latte".into())
        );
        assert_eq!(
            themes("ROOST-DARK", "github light default"),
            ("roost-dark".into(), "GitHub Light Default".into())
        );
        assert_eq!(
            themes("Catppuccin Latte", "Catppuccin Mocha"),
            ("roost-dark".into(), "Catppuccin Latte".into()),
            "a light name in dark_theme and a dark one in light_theme are replaced"
        );
        assert_eq!(
            themes("", "no such theme"),
            ("roost-dark".into(), "Catppuccin Latte".into())
        );
    }

    #[test]
    fn malformed_settings_are_the_defaults_and_kept_as_bad() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_settings(dir.path(), "{\"appearance\": ");
        assert_eq!(
            load_settings(dir.path()).expect("recovered"),
            MobileSettings::default()
        );
        assert!(!dir.path().join(SETTINGS_FILE).exists());
        let kept = std::fs::read_to_string(dir.path().join(SETTINGS_BAD_FILE)).expect(".bad");
        assert_eq!(kept, "{\"appearance\": ");
    }

    #[test]
    fn unreadable_settings_are_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join(SETTINGS_FILE)).expect("a directory in the way");
        let error = load_settings(dir.path()).expect_err("not NotFound, not malformed");
        assert_ne!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(!dir.path().join(SETTINGS_BAD_FILE).exists());
    }

    #[test]
    fn targets_parse_into_unix_paths_or_ssh_logins() {
        assert_eq!(
            Target::parse("unix:/run/roost.sock"),
            Ok(Target::Unix("/run/roost.sock".into()))
        );
        let Ok(Target::Ssh(ssh)) = Target::parse("charlie@popos:2222") else {
            panic!("an ssh target");
        };
        assert_eq!(
            (ssh.user.as_str(), ssh.host.as_str(), ssh.port),
            ("charlie", "popos", 2222)
        );
        let missing = Target::parse("popos").expect_err("no user");
        assert!(missing.contains("user@host"), "{missing}");
        assert!(Target::parse("unix:").is_err());
        assert!(Target::parse("").is_err());
    }
}
