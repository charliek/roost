//! A development host for the mobile app: an SSH server that stands in
//! for Tailscale SSH (any user, the `none` login) and for `client-bridge`
//! (every exec piped to one session socket). See `roost_ssh::devserver`.
//!
//! ```text
//! cargo run -p roost-ssh --example dev_sshd -- \
//!     --key <host key file> --socket <session socket> [--listen 127.0.0.1:2222]
//! ```
//!
//! `tools/mobile/dev-host.sh` runs it beside a private dev session.
//! Anyone who can reach `--listen` gets that session's shells with no
//! password: keep it on loopback (the emulator's 10.0.2.2, or a phone
//! through `adb reverse`) unless the network is yours alone.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use roost_ssh::devserver;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

const USAGE: &str =
    "usage: dev_sshd --key <host key file> --socket <session socket> [--listen <addr:port>]";

struct Args {
    listen: SocketAddr,
    key: PathBuf,
    socket: PathBuf,
}

impl Args {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut listen = SocketAddr::from(([127, 0, 0, 1], 2222));
        let (mut key, mut socket) = (None, None);
        while let Some(flag) = args.next() {
            let mut value = || args.next().ok_or(format!("{flag} needs a value"));
            match flag.as_str() {
                "--listen" => {
                    let value = value()?;
                    listen = value
                        .parse()
                        .map_err(|_| format!("--listen {value:?} is not an addr:port"))?;
                }
                "--key" => key = Some(PathBuf::from(value()?)),
                "--socket" => socket = Some(PathBuf::from(value()?)),
                other => return Err(format!("unknown argument {other:?}")),
            }
        }
        Ok(Self {
            listen,
            key: key.ok_or("--key is required")?,
            socket: socket.ok_or("--socket is required")?,
        })
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("dev_sshd: {error}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if let Err(error) = run(args).await {
        eprintln!("dev_sshd: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn run(args: Args) -> std::io::Result<()> {
    let key = devserver::load_or_create_key(&args.key).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("host key {}: {error}", args.key.display()),
        )
    })?;
    let listener = TcpListener::bind(args.listen).await?;
    println!("dev_sshd: listening on {}", listener.local_addr()?);
    println!(
        "dev_sshd: host key {} ({})",
        devserver::fingerprint(&key),
        args.key.display()
    );
    println!("dev_sshd: every exec is piped to {}", args.socket.display());
    devserver::serve(listener, key, args.socket).await
}
