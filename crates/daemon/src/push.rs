//! Notifications on the paired phones (Web Push). A phone's browser gives
//! the daemon its push subscription over the encrypted channel; to notify
//! it, the daemon asks the relay, which signs the request for the push
//! service (VAPID) and passes the title and text on without keeping them.

use crate::{
    hub::{lock, Hub},
    store::write_private,
};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
};

const FILE: &str = "push.json";

pub struct Push {
    path: PathBuf,
    /// `{device, endpoint, keys: {p256dh, auth}}`, one per browser.
    subscriptions: Mutex<Vec<Value>>,
}

impl Push {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join(FILE);
        let subscriptions = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        Self {
            path,
            subscriptions: Mutex::new(subscriptions),
        }
    }

    /// Keeps `subscription` (a browser's `PushSubscription.toJSON()`) for
    /// `device`, replacing an earlier one for the same endpoint.
    pub fn subscribe(&self, device: &str, subscription: &Value) -> Result<(), String> {
        let endpoint = subscription["endpoint"].as_str().unwrap_or_default();
        let keys = &subscription["keys"];
        if !endpoint.starts_with("https://")
            || keys["p256dh"].as_str().is_none()
            || keys["auth"].as_str().is_none()
        {
            return Err("push_subscribe requires a push subscription".to_string());
        }
        let entry = json!({
            "device": device,
            "endpoint": endpoint,
            "keys": { "p256dh": keys["p256dh"], "auth": keys["auth"] },
        });
        let mut list = lock(&self.subscriptions);
        list.retain(|s| s["endpoint"] != endpoint);
        list.push(entry);
        self.save(&list)
    }

    pub fn unsubscribe(&self, endpoint: &str) -> Result<(), String> {
        let mut list = lock(&self.subscriptions);
        list.retain(|s| s["endpoint"] != endpoint);
        self.save(&list)
    }

    /// A revoked device's browsers get nothing more.
    pub fn forget_device(&self, device: &str) {
        let mut list = lock(&self.subscriptions);
        list.retain(|s| s["device"] != device);
        let _ = self.save(&list);
    }

    fn save(&self, list: &[Value]) -> Result<(), String> {
        write_private(&self.path, format!("{:#}\n", json!(list)).as_bytes())
            .map_err(|error| error.to_string())
    }
}

/// Notifies every subscribed browser, in the background. `tag` groups the
/// notifications of one thing (a later one replaces it); a browser whose
/// subscription expired is forgotten.
pub fn notify(hub: &Arc<Hub>, title: &str, body: &str, tag: &str) {
    let list = lock(&hub.push.subscriptions).clone();
    for subscription in list {
        let hub = Arc::clone(hub);
        let payload = json!({
            "title": title,
            "body": clip(body, 300),
            "tag": tag,
            "url": "/remote",
        });
        // 410: the relay says the push service dropped the subscription.
        thread::spawn(move || match hub.relay.push(&subscription, &payload) {
            Ok(410) => {
                let endpoint = subscription["endpoint"].as_str().unwrap_or_default();
                let _ = hub.push.unsubscribe(endpoint);
            }
            Ok(status) if status >= 300 => {
                jucode_agent_core::log_warn!("daemon", "push refused", status = status);
            }
            Ok(_) => {}
            Err(error) => {
                jucode_agent_core::log_warn!("daemon", "push failed", error = error);
            }
        });
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
