//! The jujutsu half of [`super`].
//!
//! Only reached for a card whose project is a *colocated* jj repo, which is
//! what keeps this small: the objects, `refs/heads` and the base ref are all
//! git's and are handled by [`super::git`]. What is here is making a workspace,
//! taking it away, and the one read git cannot answer for a checkout that has
//! no `.git` — where the agent's commits have got to.

use std::path::Path;

use anyhow::{bail, Context as _, Result};
use tokio::process::Command;

use crate::config::Settings;
use crate::vcs::git::{self, Rev};

/// Runs a jj command against the repo or workspace at `at`, and returns trimmed
/// stdout.
///
/// `pub(super)` so the dispatch module's tests can drive a real workspace the
/// way an agent would, rather than reaching for `Command` themselves.
pub(super) async fn run(at: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("jj")
        .arg("--repository")
        .arg(at)
        .args(args)
        .output()
        .await
        // A card only reaches here because its project has a `.jj`, which says
        // nothing about `jj` being installed — so name what is missing rather
        // than letting a bare ENOENT surface as "creating the workspace".
        .with_context(|| format!("running jj (is it on PATH?): jj {}", args.join(" ")))?;

    if !out.status.success() {
        bail!(
            "jj {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_owned())
}

/// Adds a workspace for the card and records where its diffs start from.
pub async fn create(
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

    // As generously as `git::create_worktree` reads it: a base is anything git
    // resolves, so a card started on a tag or a pasted sha must not be one jj
    // alone turns down. A bookmark in a colocated repo is a branch, and so
    // resolves here like any other.
    let base_sha = git::rev_parse(repo, Rev::Ref(base_branch))
        .await
        .with_context(|| format!("{base_branch} does not name a revision"))?;

    // NB: the ref first, for the reason `git::create_worktree` gives: the
    // workspace appearing is what tells the rest of the app the card has one,
    // so a workspace without a base would be a window where there is something
    // to diff and nowhere to diff it from.
    git::run(
        repo,
        &["update-ref", &settings.base_ref(card_id), &base_sha],
    )
    .await?;

    // NB: both steps below can leave a workspace on disk, so they share one
    // cleanup. A half-made one is not inert: `super::exists` would read it as
    // live and the next start would skip creating it, and one whose `.jj` is not
    // excluded puts jj's own state in every diff the card ever shows.
    let made = async {
        // NB: no `--ignore-working-copy`. `workspace add` refuses it outright —
        // it has to check the files out.
        run(
            repo,
            &[
                "workspace",
                "add",
                "--name",
                &settings.workspace_name(card_id),
                "-r",
                &base_sha,
                &path.to_string_lossy(),
            ],
        )
        .await?;
        exclude_own_state(path)
    }
    .await;

    if let Err(err) = made {
        remove(settings, repo, path, card_id).await;
        return Err(err);
    }

    Ok(base_sha)
}

/// Keeps jj's own state out of every diff.
///
/// Staging a workspace runs `git add -A`, which has no instinct for `.jj` the
/// way it has for `.git` — so without this each turn snapshot and working tree
/// would carry `.jj/working_copy` and the review pane would show it.
///
/// A `.gitignore` *inside* `.jj` rather than `info/exclude`: that file is the
/// user's own and shared by every worktree of the repo. This one is jj's own
/// move — it writes the same thing when it colocates — and goes away with the
/// workspace it belongs to.
fn exclude_own_state(path: &Path) -> Result<()> {
    std::fs::write(path.join(".jj/.gitignore"), "/*\n")
        .context("excluding .jj from the staged tree")
}

/// Forgets the workspace and reclaims its directory. Best-effort.
pub async fn remove(settings: &Settings, repo: &Path, path: &Path, card_id: i64) {
    let _ = run(
        repo,
        &[
            "--ignore-working-copy",
            "workspace",
            "forget",
            &settings.workspace_name(card_id),
        ],
    )
    .await;
    // jj leaves the directory where it is; it is ours to reclaim.
    let _ = std::fs::remove_dir_all(path);
}

/// Where the workspace's commits have got to.
///
/// `@-` rather than `@`, and deliberately: a workspace created at the base has
/// `@` as a fresh empty child of it, so `@-` is the base and `base..@-` is empty
/// — exactly what a detached git worktree that has committed nothing reports.
/// `@` would park an empty commit in the anchor picker whose sha moved on every
/// edit.
pub async fn head(path: &Path) -> Option<String> {
    // NB: `latest(@-)`, not `@-`. A merge working copy — `jj new a b` — has two
    // parents, and the template is rendered once per matched revision with
    // nothing between them: `@-` would hand back eighty characters of
    // concatenated ids that every downstream `merge-base` and `log` then
    // rejects without saying why. `latest` is one revision by construction.
    //
    // For a merge that is the newest parent, so the other side drops out of the
    // commit list. The diff itself is unaffected — it reads the staged working
    // tree rather than any head — and a wrong-but-well-formed range beats a
    // malformed one.
    //
    // NB: `--ignore-working-copy` is not a speedup either. Without it every read
    // snapshots the working copy, which rewrites `.jj` — waking the worktree
    // watcher — and races the agent's own jj commands.
    run(
        path,
        &[
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            "latest(@-)",
            "-T",
            "commit_id",
        ],
    )
    .await
    .ok()
    .filter(|sha| !sha.is_empty())
}

