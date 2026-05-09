use clap::Parser;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "roost-core", version, about = "Roost IPC daemon")]
struct Args {
    /// Unix domain socket path. Defaults to a runtime-dir resolved per-platform.
    #[arg(long, env = "ROOST_SOCKET")]
    socket: Option<PathBuf>,

    /// Verbosity. Pass `-v` for debug, `-vv` for trace.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let default_filter = match args.verbose {
        0 => "roost_core=info,tonic=warn",
        1 => "roost_core=debug,tonic=info",
        _ => "trace",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| default_filter.into()),
        )
        .with_target(true)
        .init();

    let socket = match args.socket {
        Some(p) => p,
        None => roost_core::runtime::default_socket_path()?,
    };

    let config = roost_core::Config {
        socket_path: socket,
    };
    roost_core::run(config).await
}
