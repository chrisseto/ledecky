//! Claude Code's workspace trust, carried from a project to its cards.
//!
//! Claude Code records trust per project root in its config file. It resolves a
//! git worktree to the main repository, so a git card has the trust of its
//! project. A jj workspace has no `.git`, so Claude Code sees a new directory
//! and shows the trust dialog for each card. No flag or hook skips that dialog.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Value};

const TRUSTED: &str = "hasTrustDialogAccepted";

/// Claude Code's config file for an agent started with `env` added to ours.
pub fn config_path(env: &std::collections::HashMap<String, String>) -> Option<PathBuf> {
    let var = |key: &str| {
        env.get(key)
            .map(PathBuf::from)
            .or_else(|| std::env::var_os(key).map(PathBuf::from))
    };
    match var("CLAUDE_CONFIG_DIR") {
        Some(dir) => Some(dir.join(".claude.json")),
        None => var("HOME").map(|home| home.join(".claude.json")),
    }
}

/// Trusts `worktree` in `config` if `repo` or one of its ancestors is trusted.
///
/// Does nothing if the file does not exist or `repo` is not trusted.
pub fn inherit(config: &Path, repo: &Path, worktree: &Path) -> Result<()> {
    let raw = match std::fs::read_to_string(config) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", config.display())),
    };
    let mut doc: Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", config.display()))?;

    let repo = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let worktree = worktree
        .canonicalize()
        .unwrap_or_else(|_| worktree.to_path_buf());
    if !grant(&mut doc, &repo, &worktree) {
        return Ok(());
    }

    // NB: a temporary file and a rename, so a running client never reads a
    // partial file. A client can still write over this change. The next start
    // makes the change again.
    let tmp = config.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(&doc)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, config).with_context(|| format!("replacing {}", config.display()))
}

/// Sets trust for `worktree` if `repo` or an ancestor has it. Returns whether
/// `doc` changed.
fn grant(doc: &mut Value, repo: &Path, worktree: &Path) -> bool {
    let trusted = |doc: &Value, path: &Path| {
        doc["projects"][path.to_string_lossy().as_ref()][TRUSTED] == json!(true)
    };
    if trusted(doc, worktree) || !repo.ancestors().any(|path| trusted(doc, path)) {
        return false;
    }

    let Some(root) = doc.as_object_mut() else {
        return false;
    };
    let projects = root.entry("projects").or_insert_with(|| json!({}));
    let Some(projects) = projects.as_object_mut() else {
        return false;
    };
    let entry = projects
        .entry(worktree.to_string_lossy())
        .or_insert_with(|| json!({}));
    let Some(entry) = entry.as_object_mut() else {
        return false;
    };
    entry.insert(TRUSTED.into(), json!(true));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(mut doc: Value) -> (bool, Value) {
        let changed = grant(&mut doc, Path::new("/src/repo"), Path::new("/data/worktrees/7"));
        (changed, doc)
    }

    #[test]
    fn a_trusted_repo_trusts_its_workspace() {
        let (changed, doc) = run(json!({
            "userID": "u",
            "projects": { "/src/repo": { TRUSTED: true, "allowedTools": [] } },
        }));
        assert!(changed);
        assert_eq!(doc["projects"]["/data/worktrees/7"][TRUSTED], json!(true));
        assert_eq!(doc["userID"], json!("u"));
        assert_eq!(doc["projects"]["/src/repo"]["allowedTools"], json!([]));
    }

    #[test]
    fn a_trusted_ancestor_counts() {
        let (changed, _) = run(json!({ "projects": { "/src": { TRUSTED: true } } }));
        assert!(changed);
    }

    #[test]
    fn an_untrusted_repo_changes_nothing() {
        let before = json!({ "projects": { "/src/repo": { TRUSTED: false } } });
        let (changed, doc) = run(before.clone());
        assert!(!changed);
        assert_eq!(doc, before);
    }

    #[test]
    fn a_trusted_workspace_is_left_alone() {
        let (changed, _) = run(json!({ "projects": {
            "/src/repo": { TRUSTED: true },
            "/data/worktrees/7": { TRUSTED: true, "lastCost": 1 },
        } }));
        assert!(!changed);
    }

    #[test]
    fn an_existing_workspace_entry_keeps_its_keys() {
        let (_, doc) = run(json!({ "projects": {
            "/src/repo": { TRUSTED: true },
            "/data/worktrees/7": { "lastCost": 1 },
        } }));
        assert_eq!(doc["projects"]["/data/worktrees/7"]["lastCost"], json!(1));
        assert_eq!(doc["projects"]["/data/worktrees/7"][TRUSTED], json!(true));
    }
}
