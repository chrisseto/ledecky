use std::path::Path;
use std::process::Command;

use anyhow::{bail, Result};

use crate::config::Settings;

/// Runs a git command in `repo` and returns trimmed stdout.
pub fn run(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(repo).args(args).output()?;

    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_owned())
}

/// Local branches, with the checked-out one first so it can be the form default.
pub fn branches(repo: &Path) -> Vec<String> {
    let head = head_branch(repo);
    let mut branches: Vec<String> = run(
        repo,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
    )
    .map(|s| s.lines().map(str::to_owned).collect())
    .unwrap_or_default();

    if let Some(head) = head {
        branches.retain(|b| *b != head);
        branches.insert(0, head);
    }
    branches
}

pub fn head_branch(repo: &Path) -> Option<String> {
    run(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .filter(|s| !s.is_empty())
}

/// Creates a detached worktree at `path` based on `base_branch`, and records the
/// starting commit as `refs/{APP_SLUG}/<card>/base` so diffs have a fixed origin.
pub fn create_worktree(
    settings: &Settings,
    repo: &Path,
    path: &Path,
    base_branch: &str,
    card_id: i64,
) -> Result<String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        bail!("{} already exists", path.display());
    }

    let base_sha = run(repo, &["rev-parse", "--verify", &format!("{base_branch}^{{commit}}")])?;

    run(
        repo,
        &["worktree", "add", "--detach", &path.to_string_lossy(), &base_sha],
    )?;
    run(repo, &["update-ref", &settings.base_ref(card_id), &base_sha])?;

    Ok(base_sha)
}

/// Tears down a card's worktree. Best-effort: a missing worktree is not an error.
pub fn remove_worktree(repo: &Path, path: &Path) {
    let _ = run(
        repo,
        &["worktree", "remove", "--force", &path.to_string_lossy()],
    );
    let _ = std::fs::remove_dir_all(path);
    let _ = run(repo, &["worktree", "prune"]);
}

fn run_env(repo: &Path, args: &[&str], envs: &[(&str, &str)]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo).args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }

    let out = cmd.output()?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_owned())
}

/// Commits the worktree's current state as `refs/{APP_SLUG}/<card>/turn-<n>`.
///
/// Returns `None` when the tree is identical to `parent`, which is how chat-only
/// turns avoid piling up empty snapshots.
///
/// NB: the staging happens in a scratch index. The worktree's real index belongs
/// to the agent — writing to it would clobber a partial `git add`, race the
/// agent's own git commands on `index.lock`, and leave our staging behind for
/// its next `git status` to report.
pub fn snapshot_turn(
    settings: &Settings,
    repo: &Path,
    worktree: &Path,
    card_id: i64,
    n: i64,
    parent: &str,
) -> Result<Option<String>> {
    let index = settings.card_dir(card_id).join("snapshot.index");
    std::fs::create_dir_all(index.parent().unwrap())?;
    let _ = std::fs::remove_file(&index);

    let index_env: &[(&str, &str)] = &[("GIT_INDEX_FILE", &index.to_string_lossy())];

    run_env(worktree, &["add", "-A"], index_env)?;
    let tree = run_env(worktree, &["write-tree"], index_env)?;
    let _ = std::fs::remove_file(&index);

    let parent_tree = run(worktree, &["rev-parse", &format!("{parent}^{{tree}}")]).ok();
    if parent_tree.as_deref() == Some(tree.as_str()) {
        return Ok(None);
    }

    let identity: &[(&str, &str)] = &[
        ("GIT_AUTHOR_NAME", &settings.app_slug),
        ("GIT_AUTHOR_EMAIL", "kanban2@localhost"),
        ("GIT_COMMITTER_NAME", &settings.app_slug),
        ("GIT_COMMITTER_EMAIL", "kanban2@localhost"),
    ];
    let sha = run_env(
        worktree,
        &["commit-tree", &tree, "-p", parent, "-m", &format!("turn {n}")],
        identity,
    )?;

    run(repo, &["update-ref", &settings.turn_ref(card_id, n), &sha])?;
    Ok(Some(sha))
}
