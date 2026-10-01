//! A stable id for this computer. Credentials and identities under
//! `~/.jucode` are only good on the computer that created them: copied to
//! another one (a migration assistant, synced dotfiles, a cloned disk) they
//! would share one JuCode device login and one relay identity, and each
//! computer would keep knocking the other off. Each records the id it was
//! made on and is dropped where the id differs.

use sha2::{Digest, Sha256};
use std::sync::OnceLock;

/// This computer's id (a hash of the OS machine id, not the id itself), or
/// None where the OS does not give one; then nothing is ever dropped.
pub fn machine_id() -> Option<&'static str> {
    static ID: OnceLock<Option<String>> = OnceLock::new();
    ID.get_or_init(|| {
        let raw = std::env::var("JUCODE_MACHINE_ID")
            .ok()
            .filter(|id| !id.trim().is_empty())
            .or_else(os_machine_id)?;
        let digest = Sha256::digest(format!("jucode-machine:{}", raw.trim()).as_bytes());
        Some(digest[..16].iter().map(|b| format!("{b:02x}")).collect())
    })
    .as_deref()
}

/// Whether something recorded on `recorded` belongs to this computer. An
/// unrecorded id (written before ids were kept) or an unknown current id
/// counts as this one.
pub fn is_this_machine(recorded: Option<&str>) -> bool {
    match (recorded, machine_id()) {
        (Some(recorded), Some(current)) => recorded == current,
        _ => true,
    }
}

#[cfg(target_os = "macos")]
fn os_machine_id() -> Option<String> {
    let out = std::process::Command::new("ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|line| line.contains("\"IOPlatformUUID\""))
        .and_then(|line| line.rsplit('"').nth(1))
        .map(str::to_string)
        .filter(|id| !id.is_empty())
}

#[cfg(windows)]
fn os_machine_id() -> Option<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("reg")
        .args([
            "query",
            r"HKLM\SOFTWARE\Microsoft\Cryptography",
            "/v",
            "MachineGuid",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|line| line.contains("MachineGuid"))
        .and_then(|line| line.split_whitespace().last())
        .map(str::to_string)
}

#[cfg(not(any(target_os = "macos", windows)))]
fn os_machine_id() -> Option<String> {
    ["/etc/machine-id", "/var/lib/dbus/machine-id"]
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrecorded_ids_count_as_this_machine() {
        assert!(is_this_machine(None));
        if let Some(id) = machine_id() {
            assert!(is_this_machine(Some(id)));
            assert!(!is_this_machine(Some("another-machine")));
            assert_eq!(id.len(), 32);
        }
    }
}
