//! Supervises the child processes behind the CLI-backed agent engines.
//!
//! One process per session, with its output tailed into a ring buffer and its
//! exit status mapped onto [`AgentStatus`]. Cancellation goes through a
//! channel rather than a shared handle on the child, because the task that
//! owns the child is the one awaiting its exit, and only one place can do
//! either.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::oneshot;
use vetinari_core::application::ports::{PortError, PortResult};
use vetinari_core::domain::agent::AgentStatus;
use vetinari_core::domain::ids::SessionId;

type Output = Arc<Mutex<VecDeque<String>>>;

#[derive(Clone)]
struct Handle {
    status: Arc<Mutex<AgentStatus>>,
    output: Output,
    stdin: Arc<tokio::sync::Mutex<Option<ChildStdin>>>,
    cancel: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

/// The child processes Vetinari has started.
pub struct ProcessRegistry {
    sessions: Mutex<BTreeMap<String, Handle>>,
    max_lines: usize,
}

impl ProcessRegistry {
    /// Build a registry keeping at most `max_lines` of output per session.
    #[must_use]
    pub fn new(max_lines: usize) -> Self {
        Self {
            sessions: Mutex::new(BTreeMap::new()),
            max_lines: max_lines.max(1),
        }
    }

    /// Start a process for a session.
    pub async fn spawn(
        &self,
        id: &SessionId,
        command: &[String],
        working_dir: &Path,
        env: &[(String, String)],
        stdin_text: Option<&str>,
    ) -> PortResult<()> {
        let (program, args) = command.split_first().ok_or_else(|| {
            PortError::not_configured("agent command", "no command was configured to run")
        })?;

        let mut child = Command::new(program)
            .args(args)
            .current_dir(working_dir)
            .envs(env.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| {
                PortError::unavailable(program.clone(), format!("could not start: {error}"))
            })?;

        let output: Output = Arc::new(Mutex::new(VecDeque::with_capacity(self.max_lines)));
        let status = Arc::new(Mutex::new(AgentStatus::Running));
        let mut stdin = child.stdin.take();

        if let Some(text) = stdin_text
            && let Some(pipe) = stdin.as_mut()
        {
            let payload = format!("{}\n", text.trim_end());
            let written = pipe
                .write_all(payload.as_bytes())
                .await
                .and(pipe.flush().await);
            if let Err(error) = written {
                prompt_write_outcome(program, &error)?;
            }
            // The prompt is the whole input, so close the pipe to let the
            // process finish rather than wait for more.
            stdin = None;
        }

        if let Some(pipe) = child.stdout.take() {
            spawn_reader_stdout(pipe, output.clone(), self.max_lines);
        }
        if let Some(pipe) = child.stderr.take() {
            spawn_reader_stderr(pipe, output.clone(), self.max_lines);
        }

        let (cancel_tx, cancel_rx) = oneshot::channel();
        let handle = Handle {
            status: status.clone(),
            output: output.clone(),
            stdin: Arc::new(tokio::sync::Mutex::new(stdin)),
            cancel: Arc::new(Mutex::new(Some(cancel_tx))),
        };
        self.sessions
            .lock()
            .expect("session lock")
            .insert(id.as_str().to_owned(), handle);

        tokio::spawn(async move {
            let outcome = tokio::select! {
                exit = child.wait() => exit.map(Some),
                _ = cancel_rx => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    Ok(None)
                }
            };
            let next = match outcome {
                Err(error) => AgentStatus::Failed {
                    message: format!("could not wait for the process: {error}"),
                },
                Ok(None) => AgentStatus::Cancelled,
                Ok(Some(exit)) if exit.success() => AgentStatus::Succeeded,
                Ok(Some(exit)) => AgentStatus::Failed {
                    message: failure_message(exit.code(), &output),
                },
            };
            *status.lock().expect("status lock") = next;
        });

        Ok(())
    }

    /// A session's status.
    pub fn status(&self, id: &SessionId) -> PortResult<AgentStatus> {
        let handle = self.handle(id)?;
        Ok(handle.status.lock().expect("status lock").clone())
    }

    /// The tail of a session's output.
    pub fn output(&self, id: &SessionId, max_lines: usize) -> PortResult<Vec<String>> {
        let handle = self.handle(id)?;
        let buffer = handle.output.lock().expect("output lock");
        let skip = buffer.len().saturating_sub(max_lines);
        Ok(buffer.iter().skip(skip).cloned().collect())
    }

    /// Send a line to a session's standard input.
    pub async fn answer(&self, id: &SessionId, answer: &str) -> PortResult<()> {
        let handle = self.handle(id)?;
        let mut guard = handle.stdin.lock().await;
        let pipe = guard.as_mut().ok_or_else(|| {
            PortError::unsupported(
                "agent session",
                "answering: its input was closed when the prompt was delivered",
            )
        })?;
        let payload = format!("{}\n", answer.trim_end());
        pipe.write_all(payload.as_bytes())
            .await
            .map_err(|error| PortError::unavailable("agent session", error.to_string()))?;
        pipe.flush()
            .await
            .map_err(|error| PortError::unavailable("agent session", error.to_string()))?;
        Ok(())
    }

    /// Stop a session's process. Finishing first is not an error.
    pub async fn cancel(&self, id: &SessionId) -> PortResult<()> {
        let handle = self.handle(id)?;
        if let Some(sender) = handle.cancel.lock().expect("cancel lock").take() {
            let _ = sender.send(());
        }
        Ok(())
    }

    /// Forget a session, reporting whether it was known.
    pub fn forget(&self, id: &SessionId) -> bool {
        self.sessions
            .lock()
            .expect("session lock")
            .remove(id.as_str())
            .is_some()
    }

    /// How many sessions are tracked.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sessions.lock().expect("session lock").len()
    }

    /// Whether any sessions are tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn handle(&self, id: &SessionId) -> PortResult<Handle> {
        self.sessions
            .lock()
            .expect("session lock")
            .get(id.as_str())
            .cloned()
            .ok_or_else(|| PortError::not_found("agent session", id.as_str()))
    }
}

/// A process that exits without reading its prompt is not a failure: the
/// prompt is also in its environment, and a tool is entitled to ignore its
/// input. Any other write error means the process never got the task.
fn prompt_write_outcome(program: &str, error: &std::io::Error) -> PortResult<()> {
    if error.kind() == std::io::ErrorKind::BrokenPipe {
        return Ok(());
    }
    Err(PortError::unavailable(
        program.to_owned(),
        format!("could not write prompt: {error}"),
    ))
}

fn failure_message(code: Option<i32>, output: &Output) -> String {
    let tail: Vec<String> = {
        let buffer = output.lock().expect("output lock");
        let skip = buffer.len().saturating_sub(5);
        buffer.iter().skip(skip).cloned().collect()
    };
    let status = match code {
        Some(code) => format!("exited with status {code}"),
        None => "was killed by a signal".to_owned(),
    };
    if tail.is_empty() {
        status
    } else {
        format!("{status}: {}", tail.join(" | "))
    }
}

fn push_line(output: &Output, max_lines: usize, line: String) {
    let mut buffer = output.lock().expect("output lock");
    if buffer.len() == max_lines {
        buffer.pop_front();
    }
    buffer.push_back(line);
}

fn spawn_reader_stdout(pipe: ChildStdout, output: Output, max_lines: usize) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(pipe).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            push_line(&output, max_lines, line);
        }
    });
}

fn spawn_reader_stderr(pipe: ChildStderr, output: Output, max_lines: usize) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(pipe).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            push_line(&output, max_lines, line);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn session(name: &str) -> SessionId {
        SessionId::parse(name).expect("id")
    }

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()]
    }

    async fn wait_for_terminal(registry: &ProcessRegistry, id: &SessionId) -> AgentStatus {
        for _ in 0..200 {
            let status = registry.status(id).expect("status");
            if status.is_terminal() {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the process never finished");
    }

    async fn wait_for_output(
        registry: &ProcessRegistry,
        id: &SessionId,
        needle: &str,
    ) -> Vec<String> {
        for _ in 0..200 {
            let lines = registry.output(id, 100).expect("output");
            if lines.iter().any(|line| line.contains(needle)) {
                return lines;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{needle:?} never appeared in the output");
    }

    #[tokio::test]
    async fn a_process_that_succeeds_reports_success_and_keeps_its_output() {
        let registry = ProcessRegistry::new(100);
        let id = session("ok-1");
        registry
            .spawn(
                &id,
                &sh("echo hello; echo world"),
                Path::new("."),
                &[],
                None,
            )
            .await
            .expect("spawn");

        assert_eq!(
            wait_for_terminal(&registry, &id).await,
            AgentStatus::Succeeded
        );
        let output = registry.output(&id, 10).expect("output");
        assert_eq!(output, vec!["hello".to_owned(), "world".to_owned()]);
    }

    #[tokio::test]
    async fn a_process_that_fails_reports_why() {
        let registry = ProcessRegistry::new(100);
        let id = session("bad-1");
        registry
            .spawn(
                &id,
                &sh("echo 'it went wrong' >&2; exit 3"),
                Path::new("."),
                &[],
                None,
            )
            .await
            .expect("spawn");

        match wait_for_terminal(&registry, &id).await {
            AgentStatus::Failed { message } => {
                assert!(message.contains("it went wrong"), "got {message:?}");
                assert!(
                    message.contains('3'),
                    "the exit status is reported: {message:?}"
                );
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stderr_is_captured_alongside_stdout() {
        let registry = ProcessRegistry::new(100);
        let id = session("mixed-1");
        registry
            .spawn(
                &id,
                &sh("echo out; echo err >&2"),
                Path::new("."),
                &[],
                None,
            )
            .await
            .expect("spawn");
        wait_for_terminal(&registry, &id).await;

        let output = registry.output(&id, 10).expect("output");
        assert!(output.iter().any(|line| line == "out"));
        assert!(output.iter().any(|line| line == "err"));
    }

    #[tokio::test]
    async fn only_the_most_recent_lines_are_kept() {
        let registry = ProcessRegistry::new(5);
        let id = session("chatty-1");
        registry
            .spawn(
                &id,
                &sh("for i in 1 2 3 4 5 6 7 8 9 10; do echo line $i; done"),
                Path::new("."),
                &[],
                None,
            )
            .await
            .expect("spawn");
        wait_for_terminal(&registry, &id).await;

        let output = registry.output(&id, 100).expect("output");
        assert_eq!(output.len(), 5);
        assert_eq!(output.last().expect("a line"), "line 10");
    }

    #[tokio::test]
    async fn the_tail_can_be_asked_for_fewer_lines_than_are_held() {
        let registry = ProcessRegistry::new(100);
        let id = session("tail-1");
        registry
            .spawn(
                &id,
                &sh("echo one; echo two; echo three"),
                Path::new("."),
                &[],
                None,
            )
            .await
            .expect("spawn");
        wait_for_terminal(&registry, &id).await;

        assert_eq!(
            registry.output(&id, 2).expect("output"),
            vec!["two".to_owned(), "three".to_owned()]
        );
    }

    #[tokio::test]
    async fn the_prompt_is_delivered_on_standard_input() {
        let registry = ProcessRegistry::new(100);
        let id = session("stdin-1");
        registry
            .spawn(
                &id,
                &sh("cat"),
                Path::new("."),
                &[],
                Some("fix the memory leak"),
            )
            .await
            .expect("spawn");

        assert_eq!(
            wait_for_terminal(&registry, &id).await,
            AgentStatus::Succeeded
        );
        assert_eq!(
            registry.output(&id, 10).expect("output"),
            vec!["fix the memory leak".to_owned()]
        );
    }

    #[tokio::test]
    async fn an_answer_reaches_a_waiting_process() {
        let registry = ProcessRegistry::new(100);
        let id = session("answer-1");
        registry
            .spawn(
                &id,
                &sh("read first; echo \"got $first\"; read second; echo \"got $second\""),
                Path::new("."),
                &[],
                None,
            )
            .await
            .expect("spawn");

        registry.answer(&id, "main").await.expect("answer");
        wait_for_output(&registry, &id, "got main").await;
        registry.answer(&id, "yes").await.expect("answer");
        wait_for_output(&registry, &id, "got yes").await;

        assert_eq!(
            wait_for_terminal(&registry, &id).await,
            AgentStatus::Succeeded
        );
    }

    #[tokio::test]
    async fn environment_variables_reach_the_process() {
        let registry = ProcessRegistry::new(100);
        let id = session("env-1");
        registry
            .spawn(
                &id,
                &sh("echo \"$VETINARI_PROFILE\""),
                Path::new("."),
                &[("VETINARI_PROFILE".to_owned(), "work".to_owned())],
                None,
            )
            .await
            .expect("spawn");
        wait_for_terminal(&registry, &id).await;

        assert_eq!(
            registry.output(&id, 1).expect("output"),
            vec!["work".to_owned()]
        );
    }

    #[tokio::test]
    async fn the_process_runs_in_the_directory_it_was_given() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = ProcessRegistry::new(100);
        let id = session("cwd-1");
        registry
            .spawn(&id, &sh("pwd"), dir.path(), &[], None)
            .await
            .expect("spawn");
        wait_for_terminal(&registry, &id).await;

        let output = registry.output(&id, 1).expect("output");
        assert!(
            output[0].ends_with(
                dir.path()
                    .file_name()
                    .expect("a directory name")
                    .to_str()
                    .expect("utf-8")
            ),
            "got {output:?}"
        );
    }

    #[tokio::test]
    async fn cancelling_stops_a_long_running_process() {
        let registry = ProcessRegistry::new(100);
        let id = session("cancel-1");
        registry
            .spawn(&id, &sh("sleep 60"), Path::new("."), &[], None)
            .await
            .expect("spawn");
        assert_eq!(registry.status(&id).expect("status"), AgentStatus::Running);

        registry.cancel(&id).await.expect("cancel");

        assert_eq!(
            wait_for_terminal(&registry, &id).await,
            AgentStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn cancelling_a_finished_process_is_not_an_error() {
        let registry = ProcessRegistry::new(100);
        let id = session("cancel-2");
        registry
            .spawn(&id, &sh("true"), Path::new("."), &[], None)
            .await
            .expect("spawn");
        wait_for_terminal(&registry, &id).await;

        registry.cancel(&id).await.expect("cancel");

        assert_eq!(
            registry.status(&id).expect("status"),
            AgentStatus::Succeeded
        );
    }

    #[tokio::test]
    async fn an_unknown_session_is_reported_rather_than_guessed_at() {
        let registry = ProcessRegistry::new(100);
        let ghost = session("ghost");
        assert!(registry.status(&ghost).is_err());
        assert!(registry.output(&ghost, 1).is_err());
        assert!(registry.answer(&ghost, "hello").await.is_err());
        assert!(registry.cancel(&ghost).await.is_err());
    }

    #[tokio::test]
    async fn a_command_that_does_not_exist_fails_to_spawn() {
        let registry = ProcessRegistry::new(100);
        let error = registry
            .spawn(
                &session("missing-1"),
                &["definitely-not-a-real-binary".to_owned()],
                Path::new("."),
                &[],
                None,
            )
            .await
            .expect_err("no such binary");
        assert!(error.is_transient());
    }

    #[tokio::test]
    async fn an_empty_command_is_refused() {
        let registry = ProcessRegistry::new(100);
        let error = registry
            .spawn(&session("empty-1"), &[], Path::new("."), &[], None)
            .await
            .expect_err("no command");
        assert!(matches!(error, PortError::NotConfigured { .. }));
    }

    #[test]
    fn a_prompt_that_cannot_be_delivered_because_the_process_left_is_not_an_error() {
        assert!(
            prompt_write_outcome("cat", &std::io::Error::from(std::io::ErrorKind::BrokenPipe))
                .is_ok()
        );
    }

    #[test]
    fn any_other_failure_to_deliver_a_prompt_is_reported() {
        let error = prompt_write_outcome(
            "cat",
            &std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        )
        .expect_err("not deliverable");
        assert!(error.is_transient());
    }

    #[tokio::test]
    async fn a_process_that_exits_without_reading_its_prompt_still_succeeds() {
        let registry = ProcessRegistry::new(10);
        for index in 0..20 {
            let id = session(&format!("quick-{index}"));
            registry
                .spawn(
                    &id,
                    &["true".to_owned()],
                    Path::new("."),
                    &[],
                    Some("a prompt nobody reads"),
                )
                .await
                .expect("spawn");
            assert_eq!(
                wait_for_terminal(&registry, &id).await,
                AgentStatus::Succeeded
            );
        }
    }

    #[tokio::test]
    async fn sessions_can_be_forgotten_once_they_are_finished() {
        let registry = ProcessRegistry::new(100);
        let id = session("forget-1");
        registry
            .spawn(&id, &sh("true"), Path::new("."), &[], None)
            .await
            .expect("spawn");
        wait_for_terminal(&registry, &id).await;

        assert!(registry.forget(&id));
        assert!(registry.status(&id).is_err());
        assert!(!registry.forget(&id));
    }
}
