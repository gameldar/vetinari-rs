//! The `vetinarid` binary: arguments, logging, then `compose::run`.

use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use vetinari_adapters::daemon::client::DaemonRuns;
use vetinari_core::domain::devflow::ids::WorkerName;
use vetinari_core::domain::devflow::template::TemplateKind;
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
    /// Start a run of a template through the running daemon, and print
    /// its id.
    Start {
        /// The template; `vetinarid templates` lists them.
        template: String,
        /// The template's input: JSON, @FILE to read it from a file, or -
        /// for stdin.
        #[arg(long, value_name = "JSON")]
        input: String,
        /// Print the daemon's reply as JSON.
        #[arg(long)]
        json: bool,
    },
    /// List the templates a run can start from, with the input each takes.
    Templates {
        /// Print the list as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    if let Some(Command::Templates { json }) = cli.command {
        print!("{}", templates(json));
        return ExitCode::SUCCESS;
    }
    if let Some(Command::Start {
        template,
        input,
        json,
    }) = &cli.command
    {
        let socket = cli.socket.unwrap_or_else(compose::default_socket);
        return match start(socket, template, input, *json).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("vetinarid: {error}");
                ExitCode::FAILURE
            }
        };
    }
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

/// A start's input as given: inline JSON, `@FILE`, or `-` for `stdin`.
fn read_input(arg: &str, stdin: &mut dyn Read) -> Result<Value, String> {
    let text = match arg {
        "-" => {
            let mut text = String::new();
            stdin
                .read_to_string(&mut text)
                .map_err(|error| format!("input from stdin: {error}"))?;
            text
        }
        _ => match arg.strip_prefix('@') {
            Some(path) => std::fs::read_to_string(path)
                .map_err(|error| format!("input from {path}: {error}"))?,
            None => arg.to_owned(),
        },
    };
    serde_json::from_str(&text).map_err(|error| format!("input is not JSON: {error}"))
}

/// Every template and the fields its input takes.
fn templates(json: bool) -> String {
    if json {
        let listed: Vec<Value> = TemplateKind::ALL
            .iter()
            .map(|template| {
                json!({
                    "name": template.name(),
                    "about": template.about(),
                    "input": template.input_fields(),
                })
            })
            .collect();
        return Value::Array(listed).to_string();
    }
    let mut text = String::new();
    for template in TemplateKind::ALL {
        text.push_str(&format!("{}\n  {}\n", template.name(), template.about()));
        for field in template.input_fields() {
            let required = if field.required {
                "required"
            } else {
                "optional"
            };
            text.push_str(&format!(
                "    {:<18} {required:<8}  {}\n",
                field.name, field.about
            ));
        }
    }
    text
}

async fn start(socket: PathBuf, template: &str, input: &str, json: bool) -> Result<(), String> {
    let input = read_input(input, &mut std::io::stdin())?;
    let runs = DaemonRuns::new(socket, format!("vetinarid {}", env!("CARGO_PKG_VERSION")));
    let run = runs
        .start_template(template, input)
        .await
        .map_err(|error| error.to_string())?;
    if json {
        println!("{}", json!({ "run": run.as_str() }));
    } else {
        println!("{run}");
    }
    Ok(())
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
    fn a_start_names_a_template_and_its_input() {
        let cli = Cli::try_parse_from([
            "vetinarid",
            "--socket",
            "/tmp/v.sock",
            "start",
            "feature-from-ticket",
            "--input",
            "{}",
            "--json",
        ])
        .expect("parses");
        assert_eq!(cli.socket, Some(PathBuf::from("/tmp/v.sock")));
        assert!(matches!(
            cli.command,
            Some(Command::Start { template, input, json: true })
                if template == "feature-from-ticket" && input == "{}"
        ));
        assert!(Cli::try_parse_from(["vetinarid", "start", "feature-from-ticket"]).is_err());
    }

    #[test]
    fn input_is_read_inline_from_a_file_or_from_stdin() {
        let mut nothing = std::io::empty();
        assert_eq!(
            read_input(r#"{"profile":"work"}"#, &mut nothing),
            Ok(json!({ "profile": "work" }))
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("input.json");
        std::fs::write(&file, r#"{"profile":"home"}"#).expect("write");
        assert_eq!(
            read_input(&format!("@{}", file.display()), &mut nothing),
            Ok(json!({ "profile": "home" }))
        );

        let mut stdin = r#"{"profile":"piped"}"#.as_bytes();
        assert_eq!(
            read_input("-", &mut stdin),
            Ok(json!({ "profile": "piped" }))
        );
    }

    #[test]
    fn input_that_is_not_json_or_a_file_that_is_missing_is_refused() {
        let mut nothing = std::io::empty();
        assert!(read_input("{profile", &mut nothing).is_err());
        let missing = read_input("@/no/such/input.json", &mut nothing).expect_err("missing");
        assert!(missing.contains("/no/such/input.json"), "{missing}");
    }

    #[test]
    fn templates_are_listed_with_their_input_fields() {
        let text = templates(false);
        for template in TemplateKind::ALL {
            assert!(text.contains(template.name()), "{text}");
            for field in template.input_fields() {
                assert!(text.contains(field.name), "{text}");
            }
        }

        let listed: Value = serde_json::from_str(&templates(true)).expect("json");
        let feature = &listed[0];
        assert_eq!(feature["name"], json!("feature-from-ticket"));
        assert_eq!(
            feature["input"][1],
            json!({ "name": "ticket.key", "required": true, "about": "The tracker's key, e.g. ENG-412" })
        );
    }

    #[test]
    fn a_page_address_off_this_machine_is_refused() {
        assert!(parsed(&["--web", "0.0.0.0:7878"]).is_err());
    }
}
