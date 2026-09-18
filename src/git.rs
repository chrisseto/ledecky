use std::path::Path;
use std::process::Command;

use anyhow::{bail, Result};

use crate::config::Settings;

/// Runs a git command in `repo` and returns trimmed stdout.
pub fn run(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()?;

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

    let base_sha = run(
        repo,
        &[
            "rev-parse",
            "--verify",
            &format!("{base_branch}^{{commit}}"),
        ],
    )?;

    // NB: the ref first. Everything that reads a card's diff measures from it,
    // and the worktree appearing is what tells the rest of the app the card has
    // one — so a worktree that exists without a base would be a window where
    // there is something to diff and nowhere to diff it from.
    run(
        repo,
        &["update-ref", &settings.base_ref(card_id), &base_sha],
    )?;
    run(
        repo,
        &[
            "worktree",
            "add",
            "--detach",
            &path.to_string_lossy(),
            &base_sha,
        ],
    )?;

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

/// Writes the worktree's current state to a tree object, via `index`.
///
/// NB: the staging happens in a scratch index, and one that lives outside the
/// worktree. The worktree's real index belongs to the agent — writing to it
/// would clobber a partial `git add`, race the agent's own git commands on
/// `index.lock`, and leave our staging behind for its next `git status` to
/// report. An index kept *inside* the tree would additionally be swept up by
/// its own `add -A`.
fn stage_tree(worktree: &Path, index: &Path) -> Result<String> {
    std::fs::create_dir_all(index.parent().unwrap())?;

    let index_env: &[(&str, &str)] = &[("GIT_INDEX_FILE", &index.to_string_lossy())];
    run_env(worktree, &["add", "-A"], index_env)?;
    run_env(worktree, &["write-tree"], index_env)
}

/// The worktree exactly as it stands, as a tree object the diff can use as a
/// revision — this is what makes uncommitted work visible.
///
/// A tree sha is already content-addressed, so an unchanged worktree yields the
/// same id every time and the caller's cache and ETag both hold. That is why
/// there is no commit here: one would have to carry a timestamp, and a signed
/// one a signature timestamp, so its sha would differ on every read.
///
/// `refs/{APP_SLUG}/<card>/working` holds the result so `gc` cannot prune it
/// out from under a reader mid-diff.
pub fn working_tree(
    settings: &Settings,
    repo: &Path,
    worktree: &Path,
    card_id: i64,
) -> Result<String> {
    // NB: this index is deliberately *not* removed afterwards, unlike the turn
    // snapshot's. It is rewritten on every poll, and git's stat cache is the
    // only thing keeping `add -A` off a full re-hash of the tree each time.
    let index = settings.card_dir(card_id).join("working.index");
    let tree = stage_tree(worktree, &index)?;

    run(repo, &["update-ref", &settings.working_ref(card_id), &tree])?;
    Ok(tree)
}

/// What a revision's tree is, for comparing against a staged worktree.
///
/// NB: `working_tree` hands back a tree, so anything asking "has the worktree
/// moved since?" has to compare trees — a commit id never equals one.
pub fn tree_of(repo: &Path, rev: &str) -> Option<String> {
    run(repo, &["rev-parse", &format!("{rev}^{{tree}}")]).ok()
}

/// One of the agent's own commits, for the review pane's anchor list.
#[derive(Debug, Clone)]
pub struct Commit {
    pub sha: String,
    /// Absent on a root commit, which is diffed against the empty tree instead.
    pub parent: Option<String>,
    pub subject: String,
    /// Committer time, which is what interleaves commits with turns.
    pub at: i64,
}

/// How many of the agent's commits the picker will list.
const MAX_COMMITS: usize = 50;

/// The agent's own commits on top of the card's base, newest first.
///
/// Best-effort: a worktree that has gone away simply has no commits to offer.
pub fn commits(repo: &Path, base: &str, head: &str) -> Vec<Commit> {
    let range = format!("{base}..{head}");
    let Ok(out) = run(
        repo,
        &[
            "log",
            "--no-decorate",
            "--format=%H%x00%P%x00%ct%x00%s",
            &range,
        ],
    ) else {
        return Vec::new();
    };

    out.lines()
        .take(MAX_COMMITS)
        .filter_map(|line| {
            let mut fields = line.split('\0');
            let sha = fields.next()?.to_owned();
            // `%P` is every parent, space separated; the first is the one a
            // diff of this commit alone is measured against.
            let parent = fields.next()?.split_whitespace().next().map(str::to_owned);
            let at = fields.next()?.parse().ok()?;
            let subject = fields.next().unwrap_or_default().to_owned();

            Some(Commit {
                sha,
                parent,
                subject,
                at,
            })
        })
        .collect()
}

/// Commits the worktree's current state as `refs/{APP_SLUG}/<card>/turn-<n>`.
///
/// Returns `None` when the tree is identical to `parent`, which is how chat-only
/// turns avoid piling up empty snapshots.
pub fn snapshot_turn(
    settings: &Settings,
    repo: &Path,
    worktree: &Path,
    card_id: i64,
    n: i64,
    parent: &str,
) -> Result<Option<String>> {
    let index = settings.card_dir(card_id).join("snapshot.index");
    let _ = std::fs::remove_file(&index);
    let tree = stage_tree(worktree, &index)?;
    let _ = std::fs::remove_file(&index);

    let parent_tree = run(worktree, &["rev-parse", &format!("{parent}^{{tree}}")]).ok();
    if parent_tree.as_deref() == Some(tree.as_str()) {
        return Ok(None);
    }

    let identity: &[(&str, &str)] = &[
        ("GIT_AUTHOR_NAME", &settings.app_slug),
        ("GIT_AUTHOR_EMAIL", "ledecky@localhost"),
        ("GIT_COMMITTER_NAME", &settings.app_slug),
        ("GIT_COMMITTER_EMAIL", "ledecky@localhost"),
    ];
    let sha = run_env(
        worktree,
        &[
            "commit-tree",
            &tree,
            "-p",
            parent,
            "-m",
            &format!("turn {n}"),
        ],
        identity,
    )?;

    run(repo, &["update-ref", &settings.turn_ref(card_id, n), &sha])?;
    Ok(Some(sha))
}

/// Lines added and removed between two revisions.
///
/// `--numstat` is one cheap git call with no highlighting behind it, which is
/// what makes it usable for every card on the board rather than only the one
/// being reviewed.
pub fn diff_stat(repo: &Path, from: &str, to: &str) -> Option<(u32, u32)> {
    let out = run(repo, &["diff", "--numstat", "--no-ext-diff", from, to]).ok()?;

    Some(out.lines().fold((0, 0), |(added, removed), line| {
        let mut fields = line.split_whitespace();
        // Binary files report `-`, which reads as nothing changed.
        let count = |field: Option<&str>| field.and_then(|f| f.parse::<u32>().ok()).unwrap_or(0);
        (added + count(fields.next()), removed + count(fields.next()))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rocket::figment::providers::Serialized;
    use std::path::PathBuf;

    /// A real repository with one commit, and settings pointed somewhere its
    /// scratch index can live.
    fn scratch(name: &str) -> (Settings, PathBuf) {
        let root = std::env::temp_dir().join(format!("ledecky-git-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-q", "--initial-branch=main"]).unwrap();
        run(&repo, &["config", "user.email", "t@example.com"]).unwrap();
        run(&repo, &["config", "user.name", "t"]).unwrap();
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        run(&repo, &["add", "-A"]).unwrap();
        run(&repo, &["commit", "-qm", "first"]).unwrap();

        let settings = Settings::from(
            &rocket::figment::Figment::new()
                .merge(Serialized::default("app_slug", "ledecky"))
                .merge(Serialized::default(
                    "data_dir",
                    root.join("data").to_string_lossy().to_string(),
                )),
        )
        .unwrap();

        (settings, repo)
    }

    /// The premise the diff cache and every `304` rest on: an unchanged
    /// worktree has to produce the same object id every time.
    #[test]
    fn staging_an_unchanged_worktree_is_the_same_tree_twice() {
        let (settings, repo) = scratch("stable");

        let once = working_tree(&settings, &repo, &repo, 1).unwrap();
        let twice = working_tree(&settings, &repo, &repo, 1).unwrap();
        assert_eq!(once, twice);

        // And the ref is reachable, so `gc` cannot take it mid-read.
        assert_eq!(
            run(&repo, &["rev-parse", &settings.working_ref(1)]).unwrap(),
            once
        );
    }

    #[test]
    fn staging_picks_up_work_the_agent_has_not_committed() {
        let (settings, repo) = scratch("dirty");

        let clean = working_tree(&settings, &repo, &repo, 1).unwrap();

        // Both an edit and a wholly new file, which `git diff HEAD` alone would
        // miss — this is what the review pane could not show before.
        std::fs::write(repo.join("a.txt"), "two\n").unwrap();
        std::fs::write(repo.join("b.txt"), "new\n").unwrap();
        let dirty = working_tree(&settings, &repo, &repo, 1).unwrap();

        assert_ne!(clean, dirty);

        let (added, removed) = diff_stat(&repo, "HEAD", &dirty).unwrap();
        assert_eq!((added, removed), (2, 1));
    }

    #[test]
    fn the_scratch_index_stays_out_of_the_worktree() {
        let (settings, repo) = scratch("index");

        let tree = working_tree(&settings, &repo, &repo, 1).unwrap();

        // An index kept inside the tree would be staged by its own `add -A`.
        let listed = run(&repo, &["ls-tree", "-r", "--name-only", &tree]).unwrap();
        assert_eq!(listed, "a.txt");
        // And the agent's own index is untouched: nothing is staged.
        assert_eq!(
            run(&repo, &["diff", "--cached", "--name-only"]).unwrap(),
            ""
        );
    }

    #[test]
    fn commits_are_the_agents_own_work_newest_first() {
        let (_, repo) = scratch("commits");
        let base = run(&repo, &["rev-parse", "HEAD"]).unwrap();

        for n in 1..=2 {
            std::fs::write(repo.join("a.txt"), format!("{n}\n")).unwrap();
            run(&repo, &["add", "-A"]).unwrap();
            run(&repo, &["commit", "-qm", &format!("work {n}")]).unwrap();
        }

        let listed = commits(&repo, &base, "HEAD");
        let subjects: Vec<_> = listed.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, ["work 2", "work 1"]);

        // The parent is carried so a commit on its own never needs `sha^`.
        assert_eq!(listed[1].parent.as_deref(), Some(base.as_str()));
        assert!(listed[0].at >= listed[1].at);

        // Nothing past the base, and a range that cannot resolve is empty
        // rather than an error.
        assert!(commits(&repo, "HEAD", "HEAD").is_empty());
        assert!(commits(&repo, &base, "no-such-ref").is_empty());
    }
}
