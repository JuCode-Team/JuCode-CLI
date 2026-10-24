//! Keeping the computer from idle sleep while work needs it. The daemon has
//! one holder for every feature: each holds keys under its own prefix
//! (`schedule:<id>`), and the computer stays awake while any key is held.
//! One feature's release never ends another's hold, and two features never
//! start two system assertions that could undo each other.
//!
//! macOS: `caffeinate -i -w <daemon pid>`, so it ends with the daemon even
//! if the daemon dies. Linux: `systemd-inhibit … cat`, which ends when the
//! daemon's end of its stdin closes. Windows: a thread that holds
//! `SetThreadExecutionState`. Only idle sleep is held off: closing the lid
//! or choosing Sleep still sleeps.

use std::{collections::BTreeSet, sync::Mutex};

/// A running system assertion; dropping it lets the computer sleep again.
type Hold = Box<dyn Send>;

pub struct Awake {
    state: Mutex<State>,
    start: fn() -> Result<Hold, String>,
}

#[derive(Default)]
struct State {
    keys: BTreeSet<String>,
    hold: Option<Hold>,
}

impl Default for Awake {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            start: start_hold,
        }
    }
}

impl Awake {
    /// The keys held under `prefix` (`schedule`) become `keys`, as
    /// `<prefix>:<key>`; other prefixes' keys stay.
    pub fn sync(&self, prefix: &str, keys: impl IntoIterator<Item = String>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let head = format!("{prefix}:");
        state.keys.retain(|key| !key.starts_with(&head));
        state
            .keys
            .extend(keys.into_iter().map(|key| format!("{head}{key}")));
        if state.keys.is_empty() {
            state.hold = None;
        } else if state.hold.is_none() {
            match (self.start)() {
                Ok(hold) => state.hold = Some(hold),
                Err(error) => jucode_agent_core::log_warn!(
                    "daemon",
                    "cannot keep the computer awake",
                    error = error
                ),
            }
        }
    }

    /// Every key held now.
    #[cfg(test)]
    pub(crate) fn keys(&self) -> Vec<String> {
        self.state.lock().unwrap().keys.iter().cloned().collect()
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
struct Process(std::process::Child);

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(target_os = "macos")]
fn start_hold() -> Result<Hold, String> {
    use std::process::{Command, Stdio};
    let child = Command::new("/usr/bin/caffeinate")
        .args(["-i", "-w", &std::process::id().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("caffeinate: {error}"))?;
    Ok(Box::new(Process(child)))
}

#[cfg(target_os = "linux")]
fn start_hold() -> Result<Hold, String> {
    use std::process::{Command, Stdio};
    let child = Command::new("systemd-inhibit")
        .args([
            "--what=idle:sleep",
            "--who=JuCode",
            "--why=A scheduled task is running",
            "--mode=block",
            "cat",
        ])
        // `cat` ends when this end closes: with the daemon, if it dies.
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("systemd-inhibit: {error}"))?;
    Ok(Box::new(Process(child)))
}

#[cfg(windows)]
fn start_hold() -> Result<Hold, String> {
    const ES_CONTINUOUS: u32 = 0x8000_0000;
    const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
    #[link(name = "kernel32")]
    extern "system" {
        fn SetThreadExecutionState(flags: u32) -> u32;
    }
    // The state belongs to the thread that set it: one thread sets it,
    // waits for the hold to drop, and clears it.
    struct Thread(Option<std::sync::mpsc::Sender<()>>);
    impl Drop for Thread {
        fn drop(&mut self) {
            self.0.take();
        }
    }
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    let (ready, started) = std::sync::mpsc::channel::<bool>();
    std::thread::spawn(move || {
        let set = unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) } != 0;
        let _ = ready.send(set);
        if set {
            let _ = stopped.recv();
            unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
        }
    });
    match started.recv() {
        Ok(true) => Ok(Box::new(Thread(Some(stop)))),
        _ => Err("SetThreadExecutionState failed".to_string()),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn start_hold() -> Result<Hold, String> {
    Err("not supported on this system".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static STARTED: AtomicUsize = AtomicUsize::new(0);
    static ENDED: AtomicUsize = AtomicUsize::new(0);

    struct Counted;
    impl Drop for Counted {
        fn drop(&mut self) {
            ENDED.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counted() -> Result<Hold, String> {
        STARTED.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Counted))
    }

    #[test]
    fn one_hold_lasts_while_any_feature_holds_a_key() {
        let awake = Awake {
            state: Mutex::default(),
            start: counted,
        };
        awake.sync("schedule", ["a".to_string(), "b".to_string()]);
        awake.sync("other", ["x".to_string()]);
        assert_eq!(STARTED.load(Ordering::SeqCst), 1);
        // A feature's sync replaces only its own keys.
        awake.sync("schedule", ["b".to_string()]);
        assert_eq!(awake.keys(), ["other:x", "schedule:b"]);
        awake.sync("schedule", []);
        assert_eq!(awake.keys(), ["other:x"]);
        assert_eq!(ENDED.load(Ordering::SeqCst), 0);
        awake.sync("other", []);
        assert_eq!(ENDED.load(Ordering::SeqCst), 1);
        awake.sync("schedule", ["c".to_string()]);
        assert_eq!(STARTED.load(Ordering::SeqCst), 2);
    }
}
