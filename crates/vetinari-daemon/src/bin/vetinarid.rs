//! The `vetinarid` binary: arguments, logging, then `compose::run`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use vetinari_core::domain::devflow::ids::WorkerName;
use vetinari_daemon::compose::{self, Settings, Source};
use vetinari_daemon::web::LoopbackAddr;

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

    /// Serve the web page on this loopback address.
    #[arg(long, value_name = "ADDR", default_value = LoopbackAddr::DEFAULT)]
    web: String,

    /// Serve no web page.
    #[arg(long, conflicts_with = "web")]
    no_web: bool,

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
    /// Print the running daemon's web page URL.
    Web,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    if matches!(cli.command, Some(Command::Web)) {
        let socket = cli.socket.unwrap_or_else(compose::default_socket);
        return match vetinari_daemon::web::read_url(&socket) {
            Ok(url) => {
                println!("{url}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!(
                    "vetinarid: no web page beside {}: {error}; is vetinarid running with one?",
                    socket.display()
                );
                ExitCode::FAILURE
            }
        };
    }
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
        web: if cli.no_web {
            None
        } else {
            Some(LoopbackAddr::parse(&cli.web)?)
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(args: &[&str]) -> Result<Settings, String> {
        let mut argv = vec!["vetinarid"];
        argv.extend(args);
        settings(Cli::try_parse_from(argv).map_err(|error| error.to_string())?)
    }

    #[test]
    fn the_page_is_served_on_the_default_loopback_address_unless_turned_off() {
        assert_eq!(
            parsed(&[]).expect("settings").web,
            Some(LoopbackAddr::parse(LoopbackAddr::DEFAULT).expect("addr"))
        );
        assert_eq!(parsed(&["--no-web"]).expect("settings").web, None);
        assert_eq!(
            parsed(&["--web", "127.0.0.1:9000"]).expect("settings").web,
            Some(LoopbackAddr::parse("127.0.0.1:9000").expect("addr"))
        );
    }

    #[test]
    fn a_page_address_off_this_machine_is_refused() {
        assert!(parsed(&["--web", "0.0.0.0:7878"]).is_err());
    }
}
