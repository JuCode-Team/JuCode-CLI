//! `!` shell escape: input starting with `!` runs a command in the user's
//! shell on the local machine (it is never sent to the model). The command
//! runs on a background thread so the UI stays responsive; the finished
//! output is delivered through a channel polled by the main loop.

use std::{
    process::Command,
    sync::mpsc::{channel, Receiver, Sender, TryRecvError},
};

/// Cap on stored shell output so a runaway command cannot bloat the history.
const MAX_LOCAL_SHELL_OUTPUT_BYTES: usize = 20_000;

/// The shell command from a `!`-prefixed input line, if it is one.
/// A lone `!` (or `!` followed by blanks) is not a command and is treated as
/// a regular message.
pub(crate) fn local_shell_command(input: &str) -> Option<&str> {
    let command = input.strip_prefix('!')?.trim();
    if command.is_empty() {
        None
    } else {
        Some(command)
    }
}

#[derive(Debug)]
pub(crate) struct LocalShellResult {
    pub(crate) call_id: String,
    pub(crate) output: String,
}

pub(crate) struct LocalShellRunner {
    tx: Sender<LocalShellResult>,
    rx: Receiver<LocalShellResult>,
    next_id: u64,
}

impl Default for LocalShellRunner {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self { tx, rx, next_id: 0 }
    }
}

impl LocalShellRunner {
    /// Starts `command` in the user's shell and returns the call id under
    /// which its result will be delivered.
    pub(crate) fn spawn(&mut self, command: &str) -> String {
        self.next_id += 1;
        let call_id = format!("local-shell-{}", self.next_id);
        let tx = self.tx.clone();
        let command = command.to_string();
        let id = call_id.clone();
        std::thread::spawn(move || {
            let output = run_local_shell(&command);
            let _ = tx.send(LocalShellResult {
                call_id: id,
                output,
            });
        });
        call_id
    }

    pub(crate) fn poll(&mut self) -> Vec<LocalShellResult> {
        let mut results = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(result) => results.push(result),
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        results
    }
}

fn run_local_shell(command: &str) -> String {
    let output = shell_invocation(command).output();
    match output {
        Ok(output) => {
            let mut text = String::new();
            text.push_str(&String::from_utf8_lossy(&output.stdout));
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            let mut text = truncate_output(text.trim_end());
            match output.status.code() {
                Some(0) => {}
                Some(code) => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&format!("(exit {code})"));
                }
                None => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str("(killed by signal)");
                }
            }
            if text.is_empty() {
                "(no output)".to_string()
            } else {
                text
            }
        }
        Err(error) => format!("failed to start shell: {error}"),
    }
}

#[cfg(not(windows))]
fn shell_invocation(command: &str) -> Command {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let mut invocation = Command::new(shell);
    invocation.arg("-c").arg(command);
    invocation
}

#[cfg(windows)]
fn shell_invocation(command: &str) -> Command {
    let mut invocation = Command::new("cmd");
    invocation.arg("/C").arg(command);
    invocation
}

fn truncate_output(text: &str) -> String {
    if text.len() <= MAX_LOCAL_SHELL_OUTPUT_BYTES {
        return text.to_string();
    }
    let mut end = MAX_LOCAL_SHELL_OUTPUT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (output truncated)", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bang_prefix_extracts_the_shell_command() {
        assert_eq!(local_shell_command("!ls -la"), Some("ls -la"));
        assert_eq!(local_shell_command("!  git status "), Some("git status"));
        assert_eq!(local_shell_command("!"), None);
        assert_eq!(local_shell_command("!   "), None);
        assert_eq!(local_shell_command("ls"), None);
        assert_eq!(local_shell_command("hello!"), None);
    }

    #[test]
    fn runner_delivers_output_with_exit_code_annotation() {
        let mut runner = LocalShellRunner::default();
        let call_id = runner.spawn("echo hello-from-test");
        let result = wait_for_result(&mut runner);
        assert_eq!(result.call_id, call_id);
        assert!(result.output.contains("hello-from-test"), "{result:?}");
        assert!(!result.output.contains("(exit"), "{result:?}");
    }

    #[cfg(not(windows))]
    #[test]
    fn failing_command_reports_exit_code() {
        let mut runner = LocalShellRunner::default();
        runner.spawn("exit 3");
        let result = wait_for_result(&mut runner);
        assert!(result.output.contains("(exit 3)"), "{result:?}");
    }

    #[test]
    fn long_output_is_truncated() {
        let long = "x".repeat(MAX_LOCAL_SHELL_OUTPUT_BYTES + 100);
        let truncated = truncate_output(&long);
        assert!(truncated.len() < long.len());
        assert!(truncated.ends_with("… (output truncated)"));
    }

    fn wait_for_result(runner: &mut LocalShellRunner) -> LocalShellResult {
        for _ in 0..200 {
            if let Some(result) = runner.poll().pop() {
                return result;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("local shell result never arrived");
    }
}
