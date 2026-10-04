//! Notifications on the paired phones. A phone's browser gives the daemon
//! its Web Push subscription over the encrypted channel, the JuCode Android
//! app its 个推 (Getui) client id; to notify it, the daemon asks the relay,
//! which signs the request for the push service (VAPID) or sends it through
//! Getui, and passes the title and text on without keeping them.

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
    /// `{device, endpoint, keys: {p256dh, auth}}`, one per browser, or
    /// `{device, provider: "getui", client_id}`, one per Android app.
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

    /// Keeps `subscription` (a browser's `PushSubscription.toJSON()`, or
    /// `{provider: "getui", client_id}` from the Android app) for `device`,
    /// replacing an earlier one for the same endpoint or client id.
    pub fn subscribe(&self, device: &str, subscription: &Value) -> Result<(), String> {
        let entry = if subscription["provider"] == "getui" {
            let client_id = subscription["client_id"].as_str().unwrap_or_default();
            if client_id.is_empty()
                || client_id.len() > 64
                || !client_id.chars().all(|c| c.is_ascii_alphanumeric())
            {
                return Err("push_subscribe requires a getui client_id".to_string());
            }
            json!({ "device": device, "provider": "getui", "client_id": client_id })
        } else {
            let endpoint = subscription["endpoint"].as_str().unwrap_or_default();
            let keys = &subscription["keys"];
            if !endpoint.starts_with("https://")
                || keys["p256dh"].as_str().is_none()
                || keys["auth"].as_str().is_none()
            {
                return Err("push_subscribe requires a push subscription".to_string());
            }
            json!({
                "device": device,
                "endpoint": endpoint,
                "keys": { "p256dh": keys["p256dh"], "auth": keys["auth"] },
            })
        };
        let id = key(&entry).to_string();
        let mut list = lock(&self.subscriptions);
        list.retain(|s| key(s) != id);
        list.push(entry);
        self.save(&list)
    }

    /// Drops the subscription with this endpoint or Getui client id.
    pub fn unsubscribe(&self, id: &str) -> Result<(), String> {
        let mut list = lock(&self.subscriptions);
        list.retain(|s| key(s) != id);
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

/// What identifies a subscription: its endpoint, or its Getui client id.
fn key(subscription: &Value) -> &str {
    subscription["endpoint"]
        .as_str()
        .or(subscription["client_id"].as_str())
        .unwrap_or_default()
}

/// The push service a subscription goes through, for `push_tested`.
fn service(subscription: &Value) -> &str {
    if subscription["provider"] == "getui" {
        return "getui";
    }
    key(subscription).split('/').nth(2).unwrap_or_default()
}

/// `push_test`: a test notification to `device`'s browsers, sent now, with
/// each push service's answer (passed on by the relay): the phone can tell
/// whether a notification left, and through which service.
pub fn test(hub: &Arc<Hub>, device: &str) -> Value {
    let list: Vec<Value> = lock(&hub.push.subscriptions)
        .iter()
        .filter(|s| s["device"] == device)
        .cloned()
        .collect();
    let payload = json!({
        "title": "JuCode",
        "body": "测试通知：这台设备能收到电脑发来的通知。",
        "tag": "push-test",
        "url": "/remote",
    });
    let results: Vec<Value> = list
        .iter()
        .map(|subscription| {
            let service = service(subscription);
            match hub.relay.push(subscription, &payload) {
                Ok(status) => json!({ "service": service, "status": status }),
                Err(error) => json!({ "service": service, "error": error }),
            }
        })
        .collect();
    json!({ "type": "push_tested", "results": results })
}

/// Notifies every subscribed phone, in the background. `tag` groups the
/// notifications of one thing (a later one replaces it) and is the session
/// the Android app opens; a phone whose subscription expired is forgotten.
pub fn notify(hub: &Arc<Hub>, title: &str, body: &str, tag: &str) {
    send(
        hub,
        json!({ "title": title, "body": clip(body, 300), "tag": tag, "url": "/remote", "session": tag }),
    );
}

/// `notify` whose notification opens `url` on the remote page.
pub fn notify_at(hub: &Arc<Hub>, title: &str, body: &str, tag: &str, url: &str) {
    send(
        hub,
        json!({ "title": title, "body": clip(body, 300), "tag": tag, "url": url }),
    );
}

fn send(hub: &Arc<Hub>, payload: Value) {
    let list = lock(&hub.push.subscriptions).clone();
    for subscription in list {
        let hub = Arc::clone(hub);
        let payload = payload.clone();
        // 410: the relay says the push service dropped the subscription.
        thread::spawn(move || match hub.relay.push(&subscription, &payload) {
            Ok(410) => {
                let _ = hub.push.unsubscribe(key(&subscription));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_web_push_and_getui_subscriptions_per_device() {
        let dir = std::env::temp_dir().join(format!("jucode-push-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let push = Push::load(&dir);
        let web = json!({ "endpoint": "https://web.push.apple.com/a", "keys": { "p256dh": "p", "auth": "a" } });
        let phone = json!({ "provider": "getui", "client_id": "abc123" });
        push.subscribe("d1", &web).unwrap();
        push.subscribe("d1", &phone).unwrap();
        push.subscribe("d2", &phone).unwrap(); // the phone signed in again
        for bad in [
            json!({ "provider": "getui" }),
            json!({ "provider": "getui", "client_id": "a/b" }),
            json!({ "endpoint": "http://x" }),
        ] {
            assert!(push.subscribe("d1", &bad).is_err());
        }
        let list = Push::load(&dir).subscriptions.into_inner().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(
            list[1],
            json!({ "device": "d2", "provider": "getui", "client_id": "abc123" })
        );
        assert_eq!(service(&list[0]), "web.push.apple.com");
        assert_eq!(service(&list[1]), "getui");

        push.unsubscribe("abc123").unwrap();
        assert_eq!(lock(&push.subscriptions).len(), 1);
        push.subscribe("d2", &phone).unwrap();
        push.forget_device("d1");
        assert_eq!(*lock(&push.subscriptions), vec![list[1].clone()]);
        let _ = fs::remove_dir_all(&dir);
    }
}
