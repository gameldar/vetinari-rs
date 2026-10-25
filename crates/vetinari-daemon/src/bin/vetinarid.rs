//! The `vetinarid` binary: arguments, logging, then `compose::run`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use vetinari_core::domain::devflow::ids::WorkerName;
use vetinari_daemon::compose::{self, Settings, Source};

/// The variable a gateway token is read from until sign-in exists.
const TOKEN_VAR: &str = "VETINARI_AUTOGNOMUS_TOKEN";

/// Runs dev workflow steps for the autognomus engine and serves their
/// progress to vetinari.
#[derive(Debug, Parser)]
#[command(name = "vetinarid", version)]
struct Cli {
    /// Replay a recorded run instead of talking to a gateway.
    #[arg(long, value_name = "FILE", conflicts_with = "gateway")]
    scripted: Option<PathBuf>,

    /// The gateway's base URL. Its token is read from VETINARI_AUTOGNOMUS_TOKEN.
    #[arg(long, value_name = "URL")]
    gateway: Option<String>,

    /// The socket to serve; defaults to $XDG_RUNTIME_DIR/vetinari/vetinarid.sock.
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,

    /// vetinari's configuration file; defaults to the TUI's.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// The worker label; defaults to the hostname.
    #[arg(long, value_name = "NAME")]
    worker: Option<String>,

    /// Stay in the foreground. vetinarid never detaches; this is accepted so
    /// a systemd unit and a developer start it the same way.
    #[arg(long)]
    foreground: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    match settings(cli) {
        Ok(settings) => match compose::run(settings).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("vetinarid: {error}");
                ExitCode::FAILURE
            }
        },
        Err(message) => {
            eprintln!("vetinarid: {message}");
            ExitCode::from(2)
        }
    }
}

fn settings(cli: Cli) -> Result<Settings, String> {
    let worker = match cli.worker {
        Some(name) => WorkerName::parse(&name).map_err(|error| error.to_string())?,
        None => {
            let hostname = std::env::var("HOSTNAME")
                .ok()
                .or_else(|| std::fs::read_to_string("/etc/hostname").ok());
            compose::worker_from_hostname(hostname.as_deref())
        }
    };
    let source = match (cli.scripted, cli.gateway) {
        (Some(script), _) => Source::Scripted(script),
        (None, Some(url)) => Source::Gateway {
            url,
            token: std::env::var(TOKEN_VAR)
                .map_err(|_| format!("--gateway needs a token in {TOKEN_VAR}"))?,
        },
        (None, None) => return Err("give --gateway <URL> or --scripted <FILE>".to_owned()),
    };
    Ok(Settings {
        socket: cli.socket.unwrap_or_else(compose::default_socket),
        worker,
        source,
        config: cli.config,
    })
}
