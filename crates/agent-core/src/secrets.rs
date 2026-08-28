use base64::Engine as _;
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
use serde_json::Value;
use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

const ENVELOPE_PREFIX: &str = "jcenc1:";
const KEY_FILE: &str = "secret.key";
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const APP_IDENTIFIER: &str = "com.jucode.desktop";

pub(crate) type SecretKey = [u8; KEY_LEN];

/// Decrypts Desktop secret envelopes in-place. If envelopes are present, every
/// credential must decrypt with the same discovered key or loading fails.
pub(crate) fn reveal_auth(auth: &mut Value) -> io::Result<Option<SecretKey>> {
    if !contains_envelope(auth) {
        return Ok(None);
    }

    let candidates = key_candidates();
    let mut errors = Vec::new();
    let mut found_key = false;
    for path in &candidates {
        match read_key(path) {
            Ok(Some(key)) => {
                found_key = true;
                let mut revealed = auth.clone();
                match reveal_with_key(&mut revealed, &key) {
                    Ok(()) => {
                        *auth = revealed;
                        return Ok(Some(key));
                    }
                    Err(error) => errors.push(format!("{}: {error}", path.display())),
                }
            }
            Ok(None) => {}
            Err(error) => errors.push(error.to_string()),
        }
    }

    let searched = candidates
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let detail = if found_key {
        format!(
            "none of the discovered keys could decrypt it (wrong key or tampered file): {}",
            errors.join("; ")
        )
    } else if errors.is_empty() {
        format!("no secret key was found; searched: {searched}")
    } else {
        format!("no usable secret key was found: {}", errors.join("; "))
    };
    Err(invalid_data(format!(
        "failed to decrypt jcenc1 secret in auth.json: {detail}"
    )))
}

/// Finds a key for encrypting future auth writes when encryption is enabled.
/// Absence is allowed: Desktop may not have performed its first encrypted
/// write yet, in which case the CLI remains plaintext-compatible.
pub(crate) fn find_key() -> io::Result<Option<SecretKey>> {
    for path in key_candidates() {
        match read_key(&path) {
            Ok(Some(key)) => return Ok(Some(key)),
            Ok(None) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

pub(crate) fn protect_auth(auth: &mut Value, key: &SecretKey) -> io::Result<()> {
    let mut result = Ok(());
    for_each_secret(auth, |slot| {
        if result.is_err() || slot.trim().is_empty() || slot.starts_with(ENVELOPE_PREFIX) {
            return;
        }
        match encrypt(slot, key) {
            Ok(envelope) => *slot = envelope,
            Err(error) => result = Err(error),
        }
    });
    result
}

fn contains_envelope(auth: &mut Value) -> bool {
    let mut found = false;
    for_each_secret(auth, |slot| {
        found |= slot.starts_with(ENVELOPE_PREFIX);
    });
    found
}

fn reveal_with_key(auth: &mut Value, key: &SecretKey) -> io::Result<()> {
    let mut result = Ok(());
    for_each_secret(auth, |slot| {
        if result.is_err() || !slot.starts_with(ENVELOPE_PREFIX) {
            return;
        }
        match decrypt(slot, key) {
            Ok(plaintext) => *slot = plaintext,
            Err(error) => result = Err(error),
        }
    });
    result
}

fn encrypt(plaintext: &str, key: &SecretKey) -> io::Result<String> {
    let cipher = ChaCha20Poly1305::new(key.into());
    let mut nonce = [0_u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce)
        .map_err(|error| io::Error::other(format!("failed to generate secret nonce: {error}")))?;
    let sealed = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext.as_bytes())
        .map_err(|_| io::Error::other("failed to encrypt secret"))?;
    let mut blob = Vec::with_capacity(NONCE_LEN + sealed.len());
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&sealed);
    Ok(format!(
        "{ENVELOPE_PREFIX}{}",
        base64::engine::general_purpose::STANDARD.encode(blob)
    ))
}

fn decrypt(envelope: &str, key: &SecretKey) -> io::Result<String> {
    let body = envelope
        .strip_prefix(ENVELOPE_PREFIX)
        .ok_or_else(|| invalid_data("value is not a jcenc1 envelope"))?;
    let blob = base64::engine::general_purpose::STANDARD
        .decode(body)
        .map_err(|error| invalid_data(format!("malformed jcenc1 envelope: {error}")))?;
    if blob.len() <= NONCE_LEN {
        return Err(invalid_data("malformed jcenc1 envelope: truncated"));
    }
    let (nonce, sealed) = blob.split_at(NONCE_LEN);
    let cipher = ChaCha20Poly1305::new(key.into());
    let plaintext = cipher
        .decrypt(Nonce::from_slice(nonce), sealed)
        .map_err(|_| invalid_data("authentication failed"))?;
    String::from_utf8(plaintext)
        .map_err(|error| invalid_data(format!("decrypted secret is not UTF-8: {error}")))
}

fn for_each_secret(auth: &mut Value, mut visit: impl FnMut(&mut String)) {
    if let Some(providers) = auth.get_mut("providers").and_then(Value::as_object_mut) {
        for value in providers.values_mut() {
            if let Value::String(secret) = value {
                visit(secret);
            }
        }
    }
    if let Some(jucode) = auth.get_mut("jucode").and_then(Value::as_object_mut) {
        for field in ["access_token", "refresh_token"] {
            if let Some(Value::String(secret)) = jucode.get_mut(field) {
                visit(secret);
            }
        }
    }
}

fn read_key(path: &Path) -> io::Result<Option<SecretKey>> {
    match fs::read(path) {
        Ok(bytes) if bytes.len() == KEY_LEN => {
            let mut key = [0_u8; KEY_LEN];
            key.copy_from_slice(&bytes);
            Ok(Some(key))
        }
        Ok(bytes) => Err(invalid_data(format!(
            "{} is a {}-byte secret key; expected {KEY_LEN} bytes",
            path.display(),
            bytes.len()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("failed to read secret key {}: {error}", path.display()),
        )),
    }
}

fn key_candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = env::var_os("JUCODE_SECRET_KEY_PATH")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        push_unique(&mut paths, path);
    }
    if let Some(home) = home_dir() {
        push_unique(&mut paths, home.join(".jucode").join(KEY_FILE));
        if cfg!(target_os = "macos") {
            push_unique(
                &mut paths,
                home.join("Library")
                    .join("Application Support")
                    .join(APP_IDENTIFIER)
                    .join(KEY_FILE),
            );
        } else if !cfg!(windows) {
            let base = env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .unwrap_or_else(|| home.join(".config"));
            push_unique(&mut paths, base.join(APP_IDENTIFIER).join(KEY_FILE));
        }
    }
    if cfg!(windows) {
        if let Some(app_data) = env::var_os("APPDATA").map(PathBuf::from) {
            push_unique(&mut paths, app_data.join(APP_IDENTIFIER).join(KEY_FILE));
        }
    }
    paths
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn desktop_envelope_roundtrips() {
        let key = [0x42; KEY_LEN];
        let envelope = encrypt("sk-desktop-compatible", &key).unwrap();
        assert!(envelope.starts_with(ENVELOPE_PREFIX));
        assert!(!envelope.contains("sk-desktop-compatible"));
        assert_eq!(decrypt(&envelope, &key).unwrap(), "sk-desktop-compatible");
    }

    #[test]
    fn auth_roundtrip_preserves_plaintext_and_metadata() {
        let key = [0x17; KEY_LEN];
        let original = json!({
            "providers": { "openai": "sk-openai", "empty": "" },
            "jucode": {
                "access_token": "access",
                "refresh_token": "refresh",
                "access_expires_at": 123
            }
        });
        let mut auth = original.clone();
        protect_auth(&mut auth, &key).unwrap();
        assert!(auth["providers"]["openai"]
            .as_str()
            .unwrap()
            .starts_with(ENVELOPE_PREFIX));
        assert_eq!(auth["providers"]["empty"], "");
        assert_eq!(auth["jucode"]["access_expires_at"], 123);

        reveal_with_key(&mut auth, &key).unwrap();
        assert_eq!(auth, original);
    }

    #[test]
    fn wrong_key_is_a_clear_error_and_does_not_mutate_auth() {
        let mut auth = json!({ "providers": { "openai": "sk-secret" } });
        protect_auth(&mut auth, &[1; KEY_LEN]).unwrap();
        let encrypted = auth.clone();
        let error = reveal_with_key(&mut auth, &[2; KEY_LEN]).unwrap_err();
        assert!(error.to_string().contains("authentication failed"));
        assert_eq!(auth, encrypted);
    }

    #[test]
    fn plaintext_auth_needs_no_key() {
        let mut auth = json!({ "providers": { "openai": "sk-plain" } });
        assert_eq!(reveal_auth(&mut auth).unwrap(), None);
        assert_eq!(auth["providers"]["openai"], "sk-plain");
    }
}
