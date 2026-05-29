use semver::Version;
use serde_json::Value;
use std::{
    sync::mpsc::{self, Receiver},
    thread,
    time::Duration,
};

const NPM_LATEST_URL: &str = "https://registry.npmjs.org/@jucode%2Fcli/latest";
const INSTALL_COMMAND: &str = "npm i -g @jucode/cli@latest";
const UPDATE_CHECK_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub struct UpdateNotice {
    pub current_version: String,
    pub latest_version: String,
}

impl UpdateNotice {
    pub fn message(&self) -> String {
        format!(
            "update available: JuCode {} -> {}, run {}",
            self.current_version, self.latest_version, INSTALL_COMMAND
        )
    }
}

pub fn spawn_update_check(current_version: &'static str) -> Receiver<UpdateNotice> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        if let Ok(Some(notice)) = check_for_update(current_version) {
            let _ = tx.send(notice);
        }
    });
    rx
}

fn check_for_update(current_version: &str) -> Result<Option<UpdateNotice>, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(UPDATE_CHECK_TIMEOUT)
        .timeout_read(UPDATE_CHECK_TIMEOUT)
        .build();
    let value = agent
        .get(NPM_LATEST_URL)
        .set("Accept", "application/json")
        .call()
        .map_err(|error| error.to_string())?
        .into_json::<Value>()
        .map_err(|error| error.to_string())?;
    Ok(update_notice_from_registry(current_version, &value))
}

fn update_notice_from_registry(current_version: &str, value: &Value) -> Option<UpdateNotice> {
    let latest_version = value.get("version")?.as_str()?;
    if !is_newer_version(current_version, latest_version) {
        return None;
    }
    Some(UpdateNotice {
        current_version: current_version.to_string(),
        latest_version: latest_version.to_string(),
    })
}

fn is_newer_version(current_version: &str, latest_version: &str) -> bool {
    let Ok(current) = Version::parse(current_version.trim_start_matches('v')) else {
        return false;
    };
    let Ok(latest) = Version::parse(latest_version.trim_start_matches('v')) else {
        return false;
    };
    latest > current
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compares_semver_versions() {
        assert!(is_newer_version("0.1.9", "0.1.10"));
        assert!(is_newer_version("v0.1.9", "v0.2.0"));
        assert!(!is_newer_version("0.1.10", "0.1.9"));
        assert!(!is_newer_version("0.1.10", "0.1.10"));
        assert!(!is_newer_version("0.1.10", "not-a-version"));
    }

    #[test]
    fn builds_notice_only_when_registry_version_is_newer() {
        let value = json!({ "version": "0.1.4" });
        let notice = update_notice_from_registry("0.1.3", &value).unwrap();

        assert_eq!(notice.current_version, "0.1.3");
        assert_eq!(notice.latest_version, "0.1.4");
        assert!(notice.message().contains("npm i -g @jucode/cli@latest"));
        assert!(update_notice_from_registry("0.1.4", &value).is_none());
    }
}
