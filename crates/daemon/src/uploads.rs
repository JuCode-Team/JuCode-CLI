//! Files a client sends to this computer (the remote page has no file
//! system the engines can read): images and files attached to a message,
//! screenshots of a requirement. They come in chunks of base64 over the
//! client's connection, so through the relay they stay end to end encrypted
//! and other frames keep flowing between chunks; the relay stores nothing.
//!
//! A file lands in `~/.jucode/uploads/<date>/<upload id>-<name>` (outside
//! the daemon's state directory, which the jucode sandbox does not let tools
//! read) and is written as `<path>.part` until its last chunk. Leftover parts
//! and files older than `KEEP_DAYS` are removed when the daemon starts.

use crate::hub::lock;
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime},
};

/// The largest file accepted.
const MAX_FILE: u64 = 100 * 1024 * 1024;
/// The largest chunk accepted (decoded).
const MAX_CHUNK: usize = 4 * 1024 * 1024;
const KEEP_DAYS: u64 = 30;
const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "webp", "gif"];

struct Partial {
    path: PathBuf,
    name: String,
    size: u64,
}

pub struct Uploads {
    dir: PathBuf,
    partial: Mutex<HashMap<String, Partial>>,
}

impl Uploads {
    /// `jucode_dir`: `~/.jucode` (the parent of the daemon's state).
    pub fn load(jucode_dir: &Path) -> Self {
        let dir = jucode_dir.join("uploads");
        prune(&dir, Duration::from_secs(KEEP_DAYS * 24 * 60 * 60));
        Self {
            dir,
            partial: Mutex::new(HashMap::new()),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `upload`: a file's next chunk. Without `upload` it starts a file
    /// named `name`; with it, `offset` must be the size received so far.
    /// The reply to the last chunk (`last`) carries the file's path.
    pub fn receive(&self, op: &Value, id: impl FnOnce() -> String) -> Result<Value, String> {
        let data = op["data"].as_str().unwrap_or_default();
        let bytes = STANDARD
            .decode(data.trim())
            .map_err(|error| format!("bad upload data: {error}"))?;
        if bytes.len() > MAX_CHUNK {
            return Err("an upload chunk may be at most 4 MB".to_string());
        }
        let mut partial = lock(&self.partial);
        let upload = match op["upload"].as_str() {
            Some(upload) => {
                let part = partial
                    .get(upload)
                    .ok_or_else(|| format!("unknown upload {upload}"))?;
                if op["offset"].as_u64() != Some(part.size) {
                    return Err(format!("upload {upload} expects offset {}", part.size));
                }
                upload.to_string()
            }
            None => {
                let name = file_name(op["name"].as_str().unwrap_or_default());
                let upload = id();
                let day = chrono::Local::now().format("%Y-%m-%d").to_string();
                let dir = self.dir.join(day);
                fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
                let path = dir.join(format!("{upload}-{name}"));
                partial.insert(
                    upload.clone(),
                    Partial {
                        path,
                        name,
                        size: 0,
                    },
                );
                upload
            }
        };
        let part = partial.get_mut(&upload).expect("inserted or found above");
        if part.size + bytes.len() as u64 > MAX_FILE {
            let _ = fs::remove_file(part_path(&part.path));
            partial.remove(&upload);
            return Err("a file may be at most 100 MB".to_string());
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(part_path(&part.path))
            .map_err(|error| error.to_string())?;
        file.write_all(&bytes).map_err(|error| error.to_string())?;
        part.size += bytes.len() as u64;
        if op["last"] != true {
            return Ok(json!({ "type": "upload_part", "upload": upload, "size": part.size }));
        }
        let part = partial.remove(&upload).expect("present above");
        fs::rename(part_path(&part.path), &part.path).map_err(|error| error.to_string())?;
        Ok(json!({
            "type": "uploaded",
            "upload": upload,
            "path": part.path.to_string_lossy(),
            "name": part.name,
            "size": part.size,
            "image": is_image(&part.path),
        }))
    }

    /// `path` when it is a finished upload: inside the uploads directory
    /// (after resolving links), a file. Anything else a client names is
    /// refused, so no other file on this computer can be taken in.
    pub fn finished(&self, path: &str) -> Result<PathBuf, String> {
        let real = fs::canonicalize(path).map_err(|_| format!("not an upload: {path}"))?;
        let root = fs::canonicalize(&self.dir).map_err(|_| format!("not an upload: {path}"))?;
        if !real.starts_with(&root) || !real.is_file() {
            return Err(format!("not an upload: {path}"));
        }
        Ok(real)
    }
}

pub fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

fn part_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".part");
    PathBuf::from(name)
}

/// The name a client gave, as a single safe path component.
fn file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = base
        .chars()
        .map(|c| {
            if c.is_control() || ":*?\"<>|".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let clean = clean.trim().trim_start_matches('.');
    let clean: String = clean.chars().take(80).collect();
    if clean.is_empty() {
        "file".to_string()
    } else {
        clean
    }
}

/// Removes unfinished parts and anything older than `keep`.
fn prune(dir: &Path, keep: Duration) {
    let Ok(days) = fs::read_dir(dir) else { return };
    let now = SystemTime::now();
    for day in days.flatten() {
        let Ok(files) = fs::read_dir(day.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            let old = file
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|at| now.duration_since(at).ok())
                .is_some_and(|age| age >= keep);
            let part = path.extension().is_some_and(|e| e == "part");
            if (old || part) && path.is_file() {
                let _ = fs::remove_file(&path);
            }
        }
        let _ = fs::remove_dir(day.path()); // only when emptied
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("jucode-uploads-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_file_arrives_in_chunks_and_only_uploads_are_taken() {
        let root = temp("chunks");
        let uploads = Uploads::load(&root);
        let first = uploads
            .receive(
                &json!({ "name": "../a/截图 1.PNG", "data": STANDARD.encode(b"ab") }),
                || "u-1".to_string(),
            )
            .unwrap();
        assert_eq!(first["upload"], "u-1");
        assert_eq!(first["size"], 2);
        let wrong = json!({ "upload": "u-1", "offset": 0, "data": STANDARD.encode(b"c") });
        assert!(uploads.receive(&wrong, String::new).is_err());
        let last =
            json!({ "upload": "u-1", "offset": 2, "data": STANDARD.encode(b"cd"), "last": true });
        let done = uploads.receive(&last, String::new).unwrap();
        let path = done["path"].as_str().unwrap();
        assert!(path.ends_with("u-1-截图 1.PNG"));
        assert_eq!(done["image"], true);
        assert_eq!(fs::read(path).unwrap(), b"abcd");
        assert!(uploads.finished(path).is_ok());
        assert!(uploads.finished(root.join("..").to_str().unwrap()).is_err());
        let outside = root.join("secret");
        fs::write(&outside, b"x").unwrap();
        assert!(uploads.finished(outside.to_str().unwrap()).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn names_are_one_safe_component() {
        assert_eq!(file_name("../../etc/passwd"), "passwd");
        assert_eq!(file_name("C:\\x\\a:b.txt"), "a_b.txt");
        assert_eq!(file_name(".hidden"), "hidden");
        assert_eq!(file_name(""), "file");
    }

    #[test]
    fn leftover_parts_and_old_files_go_on_start() {
        let root = temp("prune");
        let day = root.join("uploads").join("2026-01-01");
        fs::create_dir_all(&day).unwrap();
        fs::write(day.join("u-1-a.txt.part"), b"x").unwrap();
        fs::write(day.join("u-2-b.txt"), b"x").unwrap();
        Uploads::load(&root);
        assert!(!day.join("u-1-a.txt.part").exists());
        assert!(day.join("u-2-b.txt").exists());
        prune(&root.join("uploads"), Duration::ZERO);
        assert!(!day.exists());
        let _ = fs::remove_dir_all(root);
    }
}
