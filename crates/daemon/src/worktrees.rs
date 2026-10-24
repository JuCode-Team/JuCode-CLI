//! Git worktrees the daemon makes for sessions (a scheduled run that works
//! apart from the main checkout). They follow the desktop's layout, so its
//! Git panel and merge tools take them as their own: the worktree is
//! `<repo parent>/.jucode-worktrees/<repo name>/<slug>` on branch
//! `jucode/<slug>`, and `branch.<branch>.jucode-base` names the branch it
//! started from.

use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
};

/// One worktree made at a time: two at once in a repository race on its
/// config lock (`git config` does not wait for it).
static CREATING: Mutex<()> = Mutex::new(());

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|error| format!("git: {error}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        let error = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(format!("git {}: {error}", args.first().unwrap_or(&"")))
    }
}

/// A new worktree for work in `cwd`, from the branch the repository has
/// checked out (its commit, when detached). `slug`: lowercase letters,
/// digits and hyphens; a taken one gets `-2`, `-3`, ….
///
/// Returns `{repo, path, branch, base, slug}` and the directory to work in:
/// `cwd`'s place inside the new worktree. Uncommitted changes in the main
/// checkout stay there.
pub fn create(cwd: &Path, slug: &str) -> Result<(Value, PathBuf), String> {
    let _one = CREATING.lock().unwrap_or_else(|poison| poison.into_inner());
    let cwd = cwd
        .canonicalize()
        .map_err(|error| format!("{}: {error}", cwd.display()))?;
    let root = git(&cwd, &["rev-parse", "--show-toplevel"]).map_err(|_| {
        format!(
            "{} 不在 git 仓库里，不能在独立 worktree 中运行",
            cwd.display()
        )
    })?;
    let root = PathBuf::from(root)
        .canonicalize()
        .map_err(|error| format!("repository root: {error}"))?;
    let name = root.file_name().ok_or("repository has no name")?;
    let container = root
        .parent()
        .ok_or("repository has no parent directory")?
        .join(".jucode-worktrees")
        .join(name);
    std::fs::create_dir_all(&container)
        .map_err(|error| format!("{}: {error}", container.display()))?;
    let base = match git(&root, &["rev-parse", "--abbrev-ref", "HEAD"])? {
        head if head == "HEAD" => git(&root, &["rev-parse", "HEAD"])?,
        branch => branch,
    };
    let taken = |s: &str| {
        container.join(s).exists()
            || git(
                &root,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/jucode/{s}"),
                ],
            )
            .is_ok()
    };
    let slug = (1..=100)
        .map(|n| match n {
            1 => slug.to_string(),
            n => format!("{slug}-{n}"),
        })
        .find(|s| !taken(s))
        .ok_or_else(|| format!("no free worktree name for {slug}"))?;
    let branch = format!("jucode/{slug}");
    let path = container.join(&slug);
    let path_text = path.display().to_string();
    git(
        &root,
        &["worktree", "add", "-b", &branch, &path_text, &base],
    )?;
    let _ = git(
        &root,
        &["config", &format!("branch.{branch}.jucode-base"), &base],
    );
    if root.join(".gitmodules").exists() {
        let _ = git(&path, &["submodule", "update", "--init", "--recursive"]);
    }
    let inside = match cwd.strip_prefix(&root) {
        Ok(rel) if !rel.as_os_str().is_empty() => path.join(rel),
        _ => path.clone(),
    };
    let tree = json!({
        "repo": root.display().to_string(),
        "path": path_text,
        "branch": branch,
        "base": base,
        "slug": slug,
    });
    Ok((tree, inside))
}

/// Removes worktree `tree` (as `create` returned it) and its branch when
/// nothing was left in them: no uncommitted or untracked files, no commit
/// past its base. One with work in it stays, as does one git cannot read.
/// Returns whether it was removed.
pub fn remove_if_unused(tree: &Value) -> bool {
    let text = |key: &str| tree[key].as_str().unwrap_or_default();
    let (repo, path, branch, base) = (
        Path::new(text("repo")),
        Path::new(text("path")),
        text("branch"),
        text("base"),
    );
    if branch.is_empty() || base.is_empty() || !path.is_dir() {
        return false;
    }
    // Whatever the repository's settings hide: work inside a submodule, and
    // untracked files, count. `--force` below skips git's own check.
    let clean = git(
        path,
        &[
            "status",
            "--porcelain",
            "--ignore-submodules=none",
            "--untracked-files=all",
        ],
    )
    .is_ok_and(|out| out.is_empty());
    let unmoved = git(repo, &["rev-list", "--count", &format!("{base}..{branch}")])
        .is_ok_and(|count| count == "0");
    if !(clean && unmoved) {
        return false;
    }
    let path_text = path.display().to_string();
    // Checked above; git refuses one with submodules without --force.
    if git(repo, &["worktree", "remove", "--force", &path_text]).is_err() {
        return false;
    }
    let _ = git(repo, &["branch", "-D", branch]);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(label: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("jucode-wt-{label}-{}", std::process::id()))
            .join("app");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
        std::fs::create_dir_all(dir.join("web")).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@t"],
            &["config", "user.name", "t"],
        ] {
            git(&dir, args).unwrap();
        }
        std::fs::write(dir.join("web/a.txt"), "a").unwrap();
        git(&dir, &["add", "."]).unwrap();
        git(&dir, &["commit", "-qm", "a"]).unwrap();
        dir
    }

    #[test]
    fn a_worktree_beside_the_repo_is_removed_only_when_unused() {
        let dir = repo("unused");
        let (tree, inside) = create(&dir.join("web"), "sch-1").unwrap();
        let root = dir.canonicalize().unwrap();
        let path = root.parent().unwrap().join(".jucode-worktrees/app/sch-1");
        assert_eq!(tree["path"], path.display().to_string());
        assert_eq!(tree["branch"], "jucode/sch-1");
        assert_eq!(tree["base"], "main");
        assert_eq!(inside, path.join("web"));
        // A second one with the same slug gets a free name.
        let (second, _) = create(&dir, "sch-1").unwrap();
        assert_eq!(second["slug"], "sch-1-2");

        // Untracked work keeps it.
        std::fs::write(path.join("new.txt"), "x").unwrap();
        assert!(!remove_if_unused(&tree));
        // So does a commit.
        git(&path, &["add", "."]).unwrap();
        git(&path, &["commit", "-qm", "x"]).unwrap();
        assert!(!remove_if_unused(&tree));
        assert!(path.is_dir());

        assert!(remove_if_unused(&second));
        assert!(!Path::new(second["path"].as_str().unwrap()).exists());
        assert!(git(
            &dir,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/heads/jucode/sch-1-2"
            ]
        )
        .is_err());
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn an_unused_worktree_with_submodules_is_removed_too() {
        let lib = repo("sub-lib");
        let dir = repo("sub-app");
        git(
            &dir,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                lib.to_str().unwrap(),
                "lib",
            ],
        )
        .unwrap();
        git(&dir, &["commit", "-qm", "lib"]).unwrap();
        // `create` checks the submodule out from a local path, which git
        // allows only when told to (in every git it runs).
        std::env::set_var("GIT_CONFIG_COUNT", "1");
        std::env::set_var("GIT_CONFIG_KEY_0", "protocol.file.allow");
        std::env::set_var("GIT_CONFIG_VALUE_0", "always");
        let (tree, _) = create(&dir, "sch-sub").unwrap();
        let path = PathBuf::from(tree["path"].as_str().unwrap());
        assert!(path.join("lib/web/a.txt").exists(), "submodule checked out");
        // Work inside a submodule the repository's settings hide still counts.
        git(&path, &["config", "submodule.lib.ignore", "all"]).unwrap();
        std::fs::write(path.join("lib/web/a.txt"), "changed").unwrap();
        assert!(!remove_if_unused(&tree));
        git(&path.join("lib"), &["checkout", "-q", "--", "."]).unwrap();
        assert!(remove_if_unused(&tree));
        assert!(!path.exists());
        for d in [dir, lib] {
            let _ = std::fs::remove_dir_all(d.parent().unwrap());
        }
    }

    #[test]
    fn a_folder_outside_git_has_no_worktree() {
        let dir = std::env::temp_dir().join(format!("jucode-wt-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(create(&dir, "sch-1")
            .unwrap_err()
            .contains("不在 git 仓库里"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
