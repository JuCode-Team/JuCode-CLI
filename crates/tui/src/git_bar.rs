//! Git branch + dirty indicator for the bottom status bar.
//!
//! The status is computed with two cheap `git` calls on a background thread
//! (refreshed every few seconds) and delivered through a channel, so the UI
//! never blocks on git — large repositories at worst show a stale value.

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError},
    time::Duration,
};

const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitStatus {
    pub(crate) branch: String,
    pub(crate) dirty: bool,
}

/// Bottom-bar segment for the git status: `" main"` when clean, `" main*"`
/// when dirty, empty outside a repository.
pub(crate) fn format_git_segment(status: Option<&GitStatus>) -> String {
    match status {
        Some(status) if status.dirty => format!(" {}*", status.branch),
        Some(status) => format!(" {}", status.branch),
        None => String::new(),
    }
}

/// Builds a `GitStatus` from raw command outputs (separated from the command
/// invocations so it can be tested without a git repository).
/// `branch` is `git rev-parse --abbrev-ref HEAD`; `short_sha` is used when the
/// head is detached; `porcelain` is `git status --porcelain`.
pub(crate) fn status_from_outputs(
    branch: &str,
    short_sha: &str,
    porcelain: &str,
) -> Option<GitStatus> {
    let branch = branch.trim();
    if branch.is_empty() {
        return None;
    }
    let label = if branch == "HEAD" {
        let sha = short_sha.trim();
        if sha.is_empty() {
            return None;
        }
        format!("({sha})")
    } else {
        branch.to_string()
    };
    Some(GitStatus {
        branch: label,
        dirty: !porcelain.trim().is_empty(),
    })
}

pub(crate) fn read_git_status(dir: &Path) -> Option<GitStatus> {
    let branch = git_stdout(dir, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let short_sha = if branch.trim() == "HEAD" {
        git_stdout(dir, &["rev-parse", "--short", "HEAD"]).unwrap_or_default()
    } else {
        String::new()
    };
    // Untracked files are excluded: scanning them is the expensive part of
    // `git status` in large trees, and tracked-file dirtiness is the signal
    // that matters for "do I have uncommitted work".
    let porcelain =
        git_stdout(dir, &["status", "--porcelain", "--untracked-files=no"]).unwrap_or_default();
    status_from_outputs(&branch, &short_sha, &porcelain)
}

fn git_stdout(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).current_dir(dir).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub(crate) struct GitStatusTracker {
    rx: Receiver<Option<GitStatus>>,
}

impl GitStatusTracker {
    pub(crate) fn start(dir: PathBuf) -> Self {
        let (tx, rx): (SyncSender<Option<GitStatus>>, _) = sync_channel(4);
        std::thread::spawn(move || loop {
            let status = read_git_status(&dir);
            match tx.try_send(status) {
                // A full channel means the UI is behind; drop this reading.
                Ok(()) | Err(std::sync::mpsc::TrySendError::Full(_)) => {}
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return,
            }
            std::thread::sleep(REFRESH_INTERVAL);
        });
        Self { rx }
    }

    /// Latest reading, if a new one arrived since the last poll.
    pub(crate) fn poll(&self) -> Option<Option<GitStatus>> {
        let mut latest = None;
        loop {
            match self.rx.try_recv() {
                Ok(status) => latest = Some(status),
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        latest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_branch_formats_without_marker() {
        let status = status_from_outputs("main\n", "", "").unwrap();
        assert_eq!(status.branch, "main");
        assert!(!status.dirty);
        assert_eq!(format_git_segment(Some(&status)), " main");
    }

    #[test]
    fn dirty_branch_gets_an_asterisk() {
        let status = status_from_outputs("feature/x\n", "", " M src/lib.rs\n").unwrap();
        assert!(status.dirty);
        assert_eq!(format_git_segment(Some(&status)), " feature/x*");
    }

    #[test]
    fn detached_head_shows_short_sha() {
        let status = status_from_outputs("HEAD\n", "abc1234\n", "").unwrap();
        assert_eq!(status.branch, "(abc1234)");
    }

    #[test]
    fn outside_a_repo_shows_nothing() {
        assert_eq!(status_from_outputs("", "", ""), None);
        assert_eq!(format_git_segment(None), "");
    }
}
