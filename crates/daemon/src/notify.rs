//! Notifications on the user's phones and browsers. The daemon posts them to
//! the JuCode backend with its own login (`POST /v1/oauth/notify`); the
//! backend sends them to every push registration of the account but the
//! daemon's own. Signed out, there is no account to notify.

use serde_json::{json, Value};
use std::{thread, time::Duration};

/// Sends one notification in the background. `host` names this computer
/// and `session` the session a tap opens.
pub fn send(host: Option<String>, title: &str, body: &str, session: Option<&str>) {
    if !jucode_agent_core::jucode_signed_in() {
        return;
    }
    let payload = payload(host, title, body, session);
    thread::spawn(move || {
        if let Err(error) = post(&payload) {
            jucode_agent_core::log_warn!("daemon", "notification failed", error = error);
        }
    });
}

fn payload(host: Option<String>, title: &str, body: &str, session: Option<&str>) -> Value {
    let mut payload = json!({ "title": title, "body": clip(body, 300) });
    if let Some(host) = host {
        payload["host"] = json!(host);
    }
    if let Some(session) = session {
        payload["session"] = json!(session);
    }
    payload
}

fn post(payload: &Value) -> Result<(), String> {
    let (api, token) = jucode_agent_core::jucode_gateway_token()?;
    let response = ureq::post(&format!("{}/v1/oauth/notify", api.trim_end_matches('/')))
        .timeout(Duration::from_secs(20))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .send_string(&payload.to_string());
    match response {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(code, _)) => Err(format!("the backend answered {code}")),
        Err(error) => Err(error.to_string()),
    }
}

fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_payload_names_only_what_it_has() {
        assert_eq!(
            payload(None, "t", "b", None),
            json!({ "title": "t", "body": "b" })
        );
        let long = "长".repeat(301);
        let full = payload(Some("h1".to_string()), "t", &long, Some("s1"));
        assert_eq!(full["host"], "h1");
        assert_eq!(full["session"], "s1");
        assert_eq!(full["body"].as_str().unwrap().chars().count(), 301);
        assert!(full["body"].as_str().unwrap().ends_with('…'));
    }
}
