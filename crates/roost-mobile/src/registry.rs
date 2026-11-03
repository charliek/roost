//! What the app keeps in its data directory: the saved hosts
//! (`hosts.json`) and this install's `client_id`. Every write is a
//! temporary file renamed over the old one, so a crash mid-write leaves
//! the previous file whole.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use roost_ssh::{HostTarget, TargetError};
use serde::{Deserialize, Serialize};

const HOSTS_FILE: &str = "hosts.json";
const CLIENT_ID_FILE: &str = "client_id";
const HOSTS_VERSION: u32 = 1;

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
