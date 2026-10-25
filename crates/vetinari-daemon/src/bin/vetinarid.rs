//! The `vetinarid` binary: arguments, logging, then `compose::run`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use vetinari_core::domain::devflow::ids::WorkerName;
use vetinari_daemon::compose::{self, Settings, Source};

/// Runs dev workflow steps for the autognomus engine and serves their
/// progress to vetinari.
#[derive(Debug, Parser)]
#[command(name = "vetinarid", version)]
struct Cli {
    /// Replay a recorded run instead of working for the gateway that
    /// `[autognomus]` names.
    #[arg(long, value_name = "FILE")]
    scripted: Option<PathBuf>,

    /// The socket to serve; defaults to $XDG_RUNTIME_DIR/vetinari/vetinarid.sock.
    #[arg(long, global = true, value_name = "PATH")]
    socket: Option<PathBuf>,

    /// vetinari's configuration file; defaults to the TUI's.
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,

    /// The worker label; defaults to the hostname.
    #[arg(long, value_name = "NAME")]
    worker: Option<String>,

    /// Stay in the foreground. vetinarid never detaches; this is accepted so
    /// a systemd unit and a developer start it the same way.
    #[arg(long)]
    foreground: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Sign in to the gateway's issuer: through the running daemon when
    /// there is one, else here, for the next start.
    Login,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    if matches!(cli.command, Some(Command::Login)) {
        let socket = cli.socket.unwrap_or_else(compose::default_socket);
        return match compose::login(socket, cli.config).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("vetinarid: {error}");
                ExitCode::FAILURE
            }
        };
    }
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
    let worker = cli
        .worker
        .map(|name| WorkerName::parse(&name).map_err(|error| error.to_string()))
        .transpose()?;
    let source = match cli.scripted {
        Some(script) => Source::Scripted(script),
        None => Source::Gateway,
    };
    Ok(Settings {
        socket: cli.socket.unwrap_or_else(compose::default_socket),
        worker,
        source,
        config: cli.config,
    })
}
