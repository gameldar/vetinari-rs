//! The Vetinari binary: terminal mechanics and the event loop.
//!
//! Everything that can be decided without a terminal lives in the library, so
//! this file is deliberately thin: set up the terminal, drive the loop, and
//! put it back the way it was found - even on a panic.

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use crossterm::event::{Event, EventStream};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::{Mutex, mpsc};
use vetinari_adapters::config::{LoadedConfig, load_or_default};
use vetinari_adapters::{Paths, Runtime};
use vetinari_core::application::config::ParseContext;
use vetinari_core::application::ports::{Clock, DeviceCodeChallenge, DeviceCodePoll};
use vetinari_core::domain::credentials::{Provider, Secret};
use vetinari_core::domain::feedback::FeedbackTarget;
use vetinari_core::domain::text::NonEmptyText;
use vetinari_tui::app::{App, Outcome, StatusMessage, Update};
use vetinari_tui::controller::Controller;
use vetinari_tui::overlay::Overlay;
use vetinari_tui::startup::{account_for, account_label, config_candidates, doctor_report};
use vetinari_tui::{event, ui};

type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

/// An orchestrator for coding agents, attention, and technical leadership.
#[derive(Debug, Parser)]
#[command(
    name = "vetinari",
    version,
    about = "An orchestrator for coding agents, attention, and technical leadership"
)]
struct Cli {
    /// Use this configuration file instead of looking for one.
    #[arg(long, short, global = true, value_name = "FILE")]
    config: Option<PathBuf>,

    /// The project feedback improves; defaults to the working directory.
    #[arg(long, global = true, value_name = "DIR")]
    project: Option<PathBuf>,

    /// Which credential account to read and write; defaults to the active
    /// profile's.
    #[arg(long, global = true, value_name = "NAME")]
    account: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Open the terminal interface. This is the default.
    Run,
    /// Check configuration, engines, and credentials.
    Doctor,
    /// Store or inspect credentials without opening the interface.
    Auth {
        /// The provider, e.g. `github`. Omit to list every provider.
        provider: Option<String>,
        /// Run the device-code flow instead of reading a token.
        #[arg(long)]
        device: bool,
        /// Forget the stored credential.
        #[arg(long)]
        forget: bool,
    },
    /// Print where configuration is looked for.
    Config,
}

#[tokio::main]
async fn main() -> Fallible<()> {
    let mut cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("VETINARI_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(io::stderr)
        .init();

    let command = cli.command.take().unwrap_or(Command::Run);
    match command {
        Command::Run => run(cli).await,
        Command::Doctor => doctor(cli).await,
        Command::Auth {
            provider,
            device,
            forget,
        } => auth(cli, provider, device, forget).await,
        Command::Config => {
            for path in candidates(&cli) {
                println!("{}", path.display());
            }
            Ok(())
        }
    }
}

fn directories() -> (Option<PathBuf>, Option<PathBuf>, Option<PathBuf>) {
    let dirs = directories::BaseDirs::new();
    let config = dirs.as_ref().map(|dirs| dirs.config_dir().to_path_buf());
    let state = dirs.as_ref().and_then(|dirs| {
        dirs.state_dir()
            .map(std::path::Path::to_path_buf)
            .or_else(|| Some(dirs.data_local_dir().to_path_buf()))
    });
    let home = dirs.as_ref().map(|dirs| dirs.home_dir().to_path_buf());
    (config, state, home)
}

fn candidates(cli: &Cli) -> Vec<PathBuf> {
    let (config_dir, _, _) = directories();
    config_candidates(cli.config.clone(), config_dir.as_deref())
}

fn load(cli: &Cli) -> Fallible<(LoadedConfig, Paths)> {
    let (config_dir, state_dir, home) = directories();
    let loaded = load_or_default(&candidates(cli), &ParseContext { home })?;
    let paths = Paths {
        config_dir: config_dir.unwrap_or_else(|| PathBuf::from(".")),
        state_dir: state_dir.unwrap_or_else(|| PathBuf::from(".")),
    };
    Ok((loaded, paths))
}

fn feedback_target(cli: &Cli) -> Fallible<FeedbackTarget> {
    let repo = match &cli.project {
        Some(path) => path.clone(),
        None => std::env::current_dir()?,
    };
    let name = repo
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_owned());
    Ok(FeedbackTarget::new(
        NonEmptyText::parse(&name)?,
        repo.canonicalize().unwrap_or(repo),
    ))
}

async fn doctor(cli: Cli) -> Fallible<()> {
    let (loaded, paths) = load(&cli)?;
    let account = account_for(cli.account.as_deref(), &loaded.config.profiles)?;
    let runtime = Runtime::build(loaded.config, paths)?;
    let missing = runtime.missing_engines().await;
    let required = runtime.required_providers();
    let statuses = runtime.auth.statuses(account.clone()).await;
    let keychain = runtime.auth.summary(account).await;

    for line in doctor_report(
        &runtime.config,
        loaded.path.as_deref(),
        &runtime.problems,
        &missing,
        &statuses,
        &required,
        Some(&keychain),
    ) {
        println!("{line}");
    }
    Ok(())
}

async fn auth(cli: Cli, provider: Option<String>, device: bool, forget: bool) -> Fallible<()> {
    let (loaded, paths) = load(&cli)?;
    let account = account_for(cli.account.as_deref(), &loaded.config.profiles)?;
    let runtime = Runtime::build(loaded.config, paths)?;

    let Some(name) = provider else {
        println!("account: {}", account_label(account.as_ref()));
        for status in runtime.auth.statuses(account).await {
            println!("{:<18} {}", status.provider.name(), status.state.label());
        }
        return Ok(());
    };
    let provider: Provider = name.parse()?;

    if forget {
        let removed = runtime.auth.forget(provider, account).await?;
        println!(
            "{}: {}",
            provider.name(),
            if removed {
                "forgotten"
            } else {
                "nothing stored"
            }
        );
        return Ok(());
    }

    if device {
        let challenge = runtime.auth.begin_device_flow(provider).await?;
        println!(
            "Visit {} and enter {}",
            challenge.verification_uri, challenge.user_code
        );
        loop {
            tokio::time::sleep(Duration::from_secs(challenge.interval_secs.max(1))).await;
            match runtime
                .auth
                .poll_device_flow(provider, account.clone(), &challenge)
                .await?
            {
                DeviceCodePoll::Pending => continue,
                DeviceCodePoll::Granted(_) => {
                    println!("{} is authenticated", provider.display_name());
                    return Ok(());
                }
                DeviceCodePoll::Rejected { reason } => {
                    return Err(format!("{}: {reason}", provider.display_name()).into());
                }
            }
        }
    }

    println!("{}", provider.auth_spec().instructions);
    if io::stdin().is_terminal() {
        print!("Paste the token for {}: ", provider.display_name());
        io::stdout().flush()?;
    }
    let mut token = String::new();
    io::stdin().read_line(&mut token)?;
    let token = token.trim();
    if token.is_empty() {
        return Err("no token was given".into());
    }
    let label = account_label(account.as_ref()).to_owned();
    runtime
        .auth
        .save_api_token(provider, account, Secret::new(token))
        .await?;
    println!(
        "stored the {} credential for the {label} account",
        provider.display_name()
    );
    Ok(())
}

async fn run(cli: Cli) -> Fallible<()> {
    let (loaded, paths) = load(&cli)?;
    let tick = Duration::from_millis(loaded.config.general.tick_rate_ms);
    let refresh_every = Duration::from_secs(loaded.config.general.refresh_secs);
    let log_lines = loaded.config.general.log_lines;
    let keymap = loaded.config.keymap.clone();
    let profiles = loaded.config.profiles.clone();
    let rules = loaded.config.queue.clone();
    let config_path = loaded.path.clone();
    let target = feedback_target(&cli)?;

    let runtime = Runtime::build(loaded.config, paths)?;
    let clock = runtime.clock.clone();
    let problems = runtime.problems.clone();
    let mut controller = Controller::new(runtime, Default::default());
    let library = controller.load_library().await;

    let mut app = App::new(
        keymap,
        profiles,
        library,
        target,
        rules,
        log_lines,
        clock.now(),
    );
    app.set_config_path(config_path);
    for problem in &problems {
        app.apply(Update::Log(format!("WARN {problem}")));
    }
    if !problems.is_empty() {
        app.apply(Update::Status(StatusMessage::warn(format!(
            "{} configured thing(s) could not be set up; see SPC a l or run vetinari doctor",
            problems.len()
        ))));
    }

    let mut terminal = setup_terminal()?;
    let result = event_loop(
        &mut terminal,
        &mut app,
        controller,
        clock,
        tick,
        refresh_every,
    )
    .await;
    restore_terminal(&mut terminal)?;
    result
}

fn setup_terminal() -> Fallible<Terminal<CrosstermBackend<io::Stdout>>> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;

    // However Vetinari ends, the terminal must be usable afterwards.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut stdout = io::stdout();
        let _ = crossterm::execute!(stdout, LeaveAlternateScreen);
        let _ = crossterm::terminal::disable_raw_mode();
        hook(info);
    }));

    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Fallible<()> {
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    crossterm::terminal::disable_raw_mode()?;
    terminal.show_cursor()?;
    Ok(())
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    controller: Controller,
    clock: Arc<dyn Clock>,
    tick: Duration,
    refresh_every: Duration,
) -> Fallible<()> {
    let controller = Arc::new(Mutex::new(controller));
    let (updates_tx, mut updates_rx) = mpsc::unbounded_channel::<Vec<Update>>();
    let mut keys = EventStream::new();
    let mut ticker = tokio::time::interval(tick);
    let mut refresher = tokio::time::interval(refresh_every);
    let mut device_poll = tokio::time::interval(Duration::from_secs(5));

    // Fill the feed before the first frame, so the developer does not look at
    // an empty inbox while the first refresh runs.
    spawn_refresh(&controller, app, &updates_tx);

    loop {
        terminal.draw(|frame| ui::render(frame, app))?;

        tokio::select! {
            Some(batch) = updates_rx.recv() => {
                for update in batch {
                    app.apply(update);
                }
            }
            _ = ticker.tick() => app.apply(Update::Tick(clock.now())),
            _ = refresher.tick() => spawn_refresh(&controller, app, &updates_tx),
            _ = device_poll.tick() => {
                if let Some((provider, challenge)) = pending_device_code(app) {
                    spawn_device_poll(&controller, app, &updates_tx, provider, challenge);
                }
            }
            Some(Ok(terminal_event)) = keys.next() => {
                match terminal_event {
                    Event::Key(key) => {
                        if let Some(chord) = event::chord_of(key) {
                            match app.on_key(chord) {
                                Outcome::Nothing => {}
                                Outcome::Quit => break,
                                Outcome::Task(task) => {
                                    let controller = controller.clone();
                                    let profile = app.profile().clone();
                                    let sender = updates_tx.clone();
                                    tokio::spawn(async move {
                                        let batch = controller.lock().await.run(task, &profile).await;
                                        let _ = sender.send(batch);
                                    });
                                }
                            }
                        }
                    }
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }
        }

        if app.should_quit() {
            break;
        }
    }
    Ok(())
}

fn spawn_refresh(
    controller: &Arc<Mutex<Controller>>,
    app: &App,
    updates: &mpsc::UnboundedSender<Vec<Update>>,
) {
    let controller = controller.clone();
    let profile = app.profile().clone();
    let sender = updates.clone();
    tokio::spawn(async move {
        let batch = controller.lock().await.refresh(&profile).await;
        let _ = sender.send(batch);
    });
}

fn spawn_device_poll(
    controller: &Arc<Mutex<Controller>>,
    app: &App,
    updates: &mpsc::UnboundedSender<Vec<Update>>,
    provider: Provider,
    challenge: DeviceCodeChallenge,
) {
    let controller = controller.clone();
    let profile = app.profile().clone();
    let sender = updates.clone();
    tokio::spawn(async move {
        let batch = controller
            .lock()
            .await
            .poll_device_flow(provider, &challenge, &profile)
            .await;
        let _ = sender.send(batch);
    });
}

fn pending_device_code(app: &App) -> Option<(Provider, DeviceCodeChallenge)> {
    match app.overlay()? {
        Overlay::DeviceCode(view) => Some((view.provider, view.challenge.clone())),
        _ => None,
    }
}
