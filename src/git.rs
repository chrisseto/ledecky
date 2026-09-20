use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Result};
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

use crate::config::Settings;

/// Runs a git command in `repo` and returns trimmed stdout.
pub async fn run(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .await?;

    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_owned())
}

/// Which of `paths` — absolute, files or directories — the worktree's own rules
/// ignore.
///
/// What a build writes is not work anyone is reviewing, and it arrives in the
/// thousands — so a burst that is entirely `target/` or `node_modules/` should
/// not cost a restage, let alone one per reader.
///
/// NB: not `run`. `check-ignore` exits 1 for "nothing matched", which is an
/// answer rather than a failure; anything else truncates the reply where git
/// stopped, and is read here as "none of them". Both err towards announcing, as
/// does its consulting the index, which keeps tracked work out of the answer
/// however the rules read.
pub async fn ignored(worktree: &Path, paths: &[PathBuf]) -> HashSet<PathBuf> {
    if paths.is_empty() {
        return HashSet::new();
    }

    let mut child = match Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["check-ignore", "-z", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            warn!("check-ignore in {}: {err}", worktree.display());
            return HashSet::new();
        }
    };

    let Some(mut stdin) = child.stdin.take() else {
        return HashSet::new();
    };

    let feed: Vec<u8> = paths
        .iter()
        .flat_map(|path| {
            let mut line = path.to_string_lossy().into_owned().into_bytes();
            line.push(0);
            line
        })
        .collect();

    // NB: written and read at the same time. check-ignore echoes the ignored
    // paths back as it reads, so feeding a batch in before collecting the answer
    // deadlocks once both pipes are full — measured at 1024 paths of ~160 bytes
    // against the usual 64 KiB, and a burst carries twice that. Asking once up
    // front instead is not open: the rules change under a live watch, and the
    // paths are whatever the burst touched.
    let write = async move {
        let _ = stdin.write_all(&feed).await;
        let _ = stdin.shutdown().await;
    };

    let (_, collected) = tokio::join!(write, child.wait_with_output());
    let Ok(out) = collected else {
        return HashSet::new();
    };

    if !matches!(out.status.code(), Some(0 | 1)) {
        warn!(
            "check-ignore in {} gave up ({}); treating nothing as ignored",
            worktree.display(),
            out.status
        );
        return HashSet::new();
    }

    // NUL on both sides, so git never has to quote a path and we never have to
    // parse the quoting back.
    out.stdout
        .split(|b| *b == 0)
        .filter(|path| !path.is_empty())
        .map(|path| PathBuf::from(String::from_utf8_lossy(path).into_owned()))
        .collect()
}

/// Whether every one of `paths` is ignored by the worktree's own rules — the
/// burst filter, asked once per settled burst by the worktree watcher.
///
/// What a build writes is not work anyone is reviewing, and it arrives in the
/// thousands — so a burst that is entirely `target/` or `node_modules/` should
/// not cost a restage, let alone one per reader.
///
/// Nothing at all is not a build, and answering "yes" would swallow the burst.
pub async fn all_ignored(worktree: &Path, paths: &[PathBuf]) -> bool {
    if paths.is_empty() {
        return false;
    }

    let ignored = ignored(worktree, paths).await;
    paths.iter().all(|path| ignored.contains(path))
}

/// Local branches, with the checked-out one first so it can be the form default.
pub async fn branches(repo: &Path) -> Vec<String> {
    let head = head_branch(repo).await;
    let mut branches: Vec<String> = run(
        repo,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
    )
    .await
    .map(|s| s.lines().map(str::to_owned).collect())
    .unwrap_or_default();

    if let Some(head) = head {
        branches.retain(|b| *b != head);
        branches.insert(0, head);
    }
    branches
}

pub async fn head_branch(repo: &Path) -> Option<String> {
    run(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .await
        .ok()
        .filter(|s| !s.is_empty())
}

/// Creates a detached worktree at `path` based on `base_branch`, and records the
/// starting commit as `refs/{APP_SLUG}/<card>/base`, which is where the card's
/// diffs are measured from until [`reconcile_base`] moves it.
pub async fn create_worktree(
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
    )
    .await?;

    // NB: the ref first. Everything that reads a card's diff measures from it,
    // and the worktree appearing is what tells the rest of the app the card has
    // one — so a worktree that exists without a base would be a window where
    // there is something to diff and nowhere to diff it from.
    run(
        repo,
        &["update-ref", &settings.base_ref(card_id), &base_sha],
    )
    .await?;
    run(
        repo,
        &[
            "worktree",
            "add",
            "--detach",
            &path.to_string_lossy(),
            &base_sha,
        ],
    )
    .await?;

    Ok(base_sha)
}

/// Tears down a card's worktree. Best-effort: a missing worktree is not an error.
pub async fn remove_worktree(repo: &Path, path: &Path) {
    let _ = run(
        repo,
        &["worktree", "remove", "--force", &path.to_string_lossy()],
    )
    .await;
    let _ = std::fs::remove_dir_all(path);
    let _ = run(repo, &["worktree", "prune"]).await;
}

/// Deletes every ref under `prefix`. Best-effort, like [`remove_worktree`].
pub async fn purge_refs(repo: &Path, prefix: &str) {
    let Ok(listed) = run(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname)",
            &format!("{prefix}**"),
        ],
    )
    .await
    else {
        return;
    };

    for name in listed.lines() {
        let _ = run(repo, &["update-ref", "-d", name]).await;
    }
}

async fn run_env(repo: &Path, args: &[&str], envs: &[(&str, &str)]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo).args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }

    let out = cmd.output().await?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_owned())
}

/// Whether `ancestor` is reachable from `descendant`.
///
/// NB: `merge-base --is-ancestor` answers with its exit status and prints
/// nothing, so [`run`] — which bails on any non-zero status — would read a plain
/// "no" as a failure and mint an error message saying git broke. This asks the
/// status directly. A rev that does not resolve exits 128 and reads as "no" too,
/// which is the right answer for the only caller: a guard that refuses to move
/// anything it cannot prove.
async fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        // `output` rather than `status` so git's complaint about a bad rev is
        // captured instead of landing in the server's stderr among pty traffic.
        .output()
        .await
        .is_ok_and(|out| out.status.success())
}

/// Moves `base_ref` up to wherever `worktree` branches from `base_branch` now.
///
/// A detached worktree does not contain new upstream commits, so a branch that
/// merely moves ahead is harmless and this does nothing. What it is for is a
/// rebase: afterwards the worktree is rooted at a commit `base_ref` has never
/// heard of, and `base_ref..worktree` would fold every upstream commit into the
/// range. Plain `merge-base` rather than `--fork-point` so that a worktree which
/// merged instead of rebasing is read the same way.
///
/// The ref only ever moves *forward*. A rewound branch, or a worktree checked
/// out at an older commit, would otherwise drag it back.
///
/// Returns the new value when it moved, and `None` otherwise — which is both the
/// ordinary case and what every failure reads as. Best-effort on purpose:
/// callers poll this, and an unresolvable `base_branch` still has to render.
pub async fn reconcile_base(
    repo: &Path,
    worktree: &Path,
    base_ref: &str,
    base_branch: &str,
) -> Option<String> {
    let current = run(
        repo,
        &["rev-parse", "--verify", &format!("{base_ref}^{{commit}}")],
    )
    .await
    .ok()?;

    // NB: in the worktree, not the repo. `HEAD` is per-worktree, and its own
    // commits are reachable from nowhere else; the repo's is whatever happens to
    // be checked out there.
    let head = run(worktree, &["rev-parse", "--verify", "HEAD^{commit}"])
        .await
        .ok()?;

    // Back in the repo, where the base branch lives. The object database and
    // `refs/heads` are shared, so this resolves either way.
    let candidate = run(repo, &["merge-base", base_branch, &head])
        .await
        .ok()
        .filter(|sha| !sha.is_empty())?;

    if candidate == current || !is_ancestor(repo, &current, &candidate).await {
        return None;
    }

    // The third argument is the expected old value, so two polls racing cannot
    // interleave into a lost update.
    run(repo, &["update-ref", base_ref, &candidate, &current])
        .await
        .ok()?;
    Some(candidate)
}

/// Writes the worktree's current state to a tree object, via `index`.
///
/// NB: the staging happens in a scratch index, and one that lives outside the
/// worktree. The worktree's real index belongs to the agent — writing to it
/// would clobber a partial `git add`, race the agent's own git commands on
/// `index.lock`, and leave our staging behind for its next `git status` to
/// report. An index kept *inside* the tree would additionally be swept up by
/// its own `add -A`.
async fn stage_tree(worktree: &Path, index: &Path) -> Result<String> {
    std::fs::create_dir_all(index.parent().unwrap())?;

    let index_env: &[(&str, &str)] = &[("GIT_INDEX_FILE", &index.to_string_lossy())];
    run_env(worktree, &["add", "-A"], index_env).await?;
    run_env(worktree, &["write-tree"], index_env).await
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
pub async fn working_tree(
    settings: &Settings,
    repo: &Path,
    worktree: &Path,
    card_id: i64,
) -> Result<String> {
    // NB: this index is deliberately *not* removed afterwards, unlike the turn
    // snapshot's. It is rewritten on every poll, and git's stat cache is the
    // only thing keeping `add -A` off a full re-hash of the tree each time.
    let index = settings.card_dir(card_id).join("working.index");
    let tree = stage_tree(worktree, &index).await?;

    run(repo, &["update-ref", &settings.working_ref(card_id), &tree]).await?;
    Ok(tree)
}

/// What a revision's tree is, for comparing against a staged worktree.
///
/// NB: `working_tree` hands back a tree, so anything asking "has the worktree
/// moved since?" has to compare trees — a commit id never equals one.
pub async fn tree_of(repo: &Path, rev: &str) -> Option<String> {
    run(repo, &["rev-parse", &format!("{rev}^{{tree}}")])
        .await
        .ok()
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
pub async fn commits(repo: &Path, base: &str, head: &str) -> Vec<Commit> {
    let range = format!("{base}..{head}");
    let Ok(out) = run(
        repo,
        &[
            "log",
            "--no-decorate",
            "--format=%H%x00%P%x00%ct%x00%s",
            &range,
        ],
    )
    .await
    else {
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
pub async fn snapshot_turn(
    settings: &Settings,
    repo: &Path,
    worktree: &Path,
    card_id: i64,
    n: i64,
    parent: &str,
) -> Result<Option<String>> {
    let index = settings.card_dir(card_id).join("snapshot.index");
    let _ = std::fs::remove_file(&index);
    let tree = stage_tree(worktree, &index).await?;
    let _ = std::fs::remove_file(&index);

    let parent_tree = run(worktree, &["rev-parse", &format!("{parent}^{{tree}}")])
        .await
        .ok();
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
    )
    .await?;

    run(repo, &["update-ref", &settings.turn_ref(card_id, n), &sha]).await?;
    Ok(Some(sha))
}

/// Lines added and removed between two revisions.
///
/// `--numstat` is one cheap git call with no highlighting behind it, which is
/// what makes it usable for every card on the board rather than only the one
/// being reviewed.
pub async fn diff_stat(repo: &Path, from: &str, to: &str) -> Option<(u32, u32)> {
    let out = run(repo, &["diff", "--numstat", "--no-ext-diff", from, to])
        .await
        .ok()?;

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
    async fn scratch(name: &str) -> (Settings, PathBuf) {
        let root = std::env::temp_dir().join(format!("ledecky-git-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-q", "--initial-branch=main"])
            .await
            .unwrap();
        run(&repo, &["config", "user.email", "t@example.com"])
            .await
            .unwrap();
        run(&repo, &["config", "user.name", "t"]).await.unwrap();
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        run(&repo, &["add", "-A"]).await.unwrap();
        run(&repo, &["commit", "-qm", "first"]).await.unwrap();

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

    /// What a build writes must not read as work to review, and one real edit
    /// among it must not be lost either.
    #[tokio::test]
    async fn a_burst_counts_as_a_build_only_when_every_path_is_ignored() {
        let (_settings, repo) = scratch("ignored").await;
        std::fs::write(repo.join(".gitignore"), "target/\n").unwrap();
        run(&repo, &["add", "-A"]).await.unwrap();
        run(&repo, &["commit", "-qm", "ignore build output"])
            .await
            .unwrap();

        let artefact = repo.join("target/debug/out.o");
        std::fs::create_dir_all(artefact.parent().unwrap()).unwrap();
        std::fs::write(&artefact, "").unwrap();

        assert!(ignored(&repo, std::slice::from_ref(&artefact))
            .await
            .contains(&artefact));

        // One tracked file in the burst is enough to make it real.
        assert!(!all_ignored(&repo, &[artefact, repo.join("a.txt")]).await);

        // Nothing at all is not a build; saying so would swallow the burst.
        assert!(!all_ignored(&repo, &[]).await);
    }

    /// What keeps a walk from pruning a directory the diff is showing: git
    /// reads tracked work under an ignored path as work, so nothing built on
    /// this can drop it.
    #[tokio::test]
    async fn an_ignored_directory_holding_tracked_work_is_not_ignored() {
        let (_settings, repo) = scratch("tracked-under-ignored").await;
        std::fs::write(repo.join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(repo.join("target")).unwrap();
        std::fs::write(repo.join("target/kept.txt"), "tracked anyway\n").unwrap();
        run(&repo, &["add", "-A", "-f"]).await.unwrap();
        run(
            &repo,
            &["commit", "-qm", "a tracked file under an ignored path"],
        )
        .await
        .unwrap();

        assert!(ignored(&repo, &[repo.join("target")]).await.is_empty());
        assert!(!all_ignored(&repo, &[repo.join("target/kept.txt")]).await);
    }

    #[tokio::test]
    async fn the_ignored_subset_comes_back_and_the_rest_does_not() {
        let (_settings, repo) = scratch("subset").await;
        std::fs::write(repo.join(".gitignore"), "target/\n").unwrap();
        run(&repo, &["add", "-A"]).await.unwrap();
        run(&repo, &["commit", "-qm", "ignore build output"])
            .await
            .unwrap();

        let artefact = repo.join("target/debug/out.o");
        let paths = [artefact.clone(), repo.join("a.txt")];
        assert_eq!(
            ignored(&repo, &paths).await,
            HashSet::from([artefact.clone()])
        );
        assert!(ignored(&repo, &[]).await.is_empty());

        // A path outside the repository stops check-ignore where it stands.
        // Whatever is left unanswered has to read as work rather than as build
        // output, or a burst carrying one would be swallowed whole.
        assert!(!all_ignored(&repo, &[PathBuf::from("/etc/hosts"), artefact]).await);
    }

    /// The premise the diff cache and every `304` rest on: an unchanged
    /// worktree has to produce the same object id every time.
    #[tokio::test]
    async fn staging_an_unchanged_worktree_is_the_same_tree_twice() {
        let (settings, repo) = scratch("stable").await;

        let once = working_tree(&settings, &repo, &repo, 1).await.unwrap();
        let twice = working_tree(&settings, &repo, &repo, 1).await.unwrap();
        assert_eq!(once, twice);

        // And the ref is reachable, so `gc` cannot take it mid-read.
        assert_eq!(
            run(&repo, &["rev-parse", &settings.working_ref(1)])
                .await
                .unwrap(),
            once
        );
    }

    #[tokio::test]
    async fn staging_picks_up_work_the_agent_has_not_committed() {
        let (settings, repo) = scratch("dirty").await;

        let clean = working_tree(&settings, &repo, &repo, 1).await.unwrap();

        // Both an edit and a wholly new file, which `git diff HEAD` alone would
        // miss — this is what the review pane could not show before.
        std::fs::write(repo.join("a.txt"), "two\n").unwrap();
        std::fs::write(repo.join("b.txt"), "new\n").unwrap();
        let dirty = working_tree(&settings, &repo, &repo, 1).await.unwrap();

        assert_ne!(clean, dirty);

        let (added, removed) = diff_stat(&repo, "HEAD", &dirty).await.unwrap();
        assert_eq!((added, removed), (2, 1));
    }

    #[tokio::test]
    async fn the_scratch_index_stays_out_of_the_worktree() {
        let (settings, repo) = scratch("index").await;

        let tree = working_tree(&settings, &repo, &repo, 1).await.unwrap();

        // An index kept inside the tree would be staged by its own `add -A`.
        let listed = run(&repo, &["ls-tree", "-r", "--name-only", &tree])
            .await
            .unwrap();
        assert_eq!(listed, "a.txt");
        // And the agent's own index is untouched: nothing is staged.
        assert_eq!(
            run(&repo, &["diff", "--cached", "--name-only"])
                .await
                .unwrap(),
            ""
        );
    }

    #[tokio::test]
    async fn commits_are_the_agents_own_work_newest_first() {
        let (_, repo) = scratch("commits").await;
        let base = run(&repo, &["rev-parse", "HEAD"]).await.unwrap();

        for n in 1..=2 {
            std::fs::write(repo.join("a.txt"), format!("{n}\n")).unwrap();
            run(&repo, &["add", "-A"]).await.unwrap();
            run(&repo, &["commit", "-qm", &format!("work {n}")])
                .await
                .unwrap();
        }

        let listed = commits(&repo, &base, "HEAD").await;
        let subjects: Vec<_> = listed.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, ["work 2", "work 1"]);

        // The parent is carried so a commit on its own never needs `sha^`.
        assert_eq!(listed[1].parent.as_deref(), Some(base.as_str()));
        assert!(listed[0].at >= listed[1].at);

        // Nothing past the base, and a range that cannot resolve is empty
        // rather than an error.
        assert!(commits(&repo, "HEAD", "HEAD").await.is_empty());
        assert!(commits(&repo, &base, "no-such-ref").await.is_empty());
    }

    /// Commits `body` to `file` in whichever tree `at` is, and returns the sha.
    async fn commit(at: &Path, file: &str, body: &str, subject: &str) -> String {
        std::fs::write(at.join(file), body).unwrap();
        run(at, &["add", "-A"]).await.unwrap();
        run(at, &["commit", "-qm", subject]).await.unwrap();
        run(at, &["rev-parse", "HEAD"]).await.unwrap()
    }

    /// The whole point of [`is_ancestor`] existing rather than `run(...).await.is_ok()`.
    #[tokio::test]
    async fn asking_whether_one_commit_is_behind_another_is_an_answer_not_an_error() {
        let (_settings, repo) = scratch("ancestor").await;
        let first = run(&repo, &["rev-parse", "HEAD"]).await.unwrap();
        let second = commit(&repo, "a.txt", "two\n", "second").await;

        assert!(is_ancestor(&repo, &first, &second).await);
        assert!(!is_ancestor(&repo, &second, &first).await);
        // A rev that does not resolve is "no", not a panic and not a hang.
        assert!(!is_ancestor(&repo, "no-such-ref", &first).await);
    }

    #[tokio::test]
    async fn a_rebase_moves_the_base_up_to_where_the_worktree_now_branches() {
        let (settings, repo) = scratch("rebase").await;
        let worktree = settings.worktree_path(1);

        let started = create_worktree(&settings, &repo, &worktree, "main", 1)
            .await
            .unwrap();
        commit(&worktree, "agent.txt", "work\n", "agent work").await;
        let upstream = commit(&repo, "upstream.txt", "theirs\n", "upstream work").await;

        // Drift on its own is not a rebase: the worktree is detached and does
        // not contain the upstream commit, so there is nothing to correct.
        assert_eq!(
            reconcile_base(&repo, &worktree, &settings.base_ref(1), "main").await,
            None
        );
        assert_eq!(
            run(&repo, &["rev-parse", &settings.base_ref(1)])
                .await
                .unwrap(),
            started
        );

        run(&worktree, &["rebase", "main"]).await.unwrap();

        assert_eq!(
            reconcile_base(&repo, &worktree, &settings.base_ref(1), "main").await,
            Some(upstream.clone())
        );
        assert_eq!(
            run(&repo, &["rev-parse", &settings.base_ref(1)])
                .await
                .unwrap(),
            upstream
        );

        // Idempotent: nothing has moved since, so there is nothing to write.
        assert_eq!(
            reconcile_base(&repo, &worktree, &settings.base_ref(1), "main").await,
            None
        );

        // What the whole change is for — the upstream commit is out of the range.
        let head = run(&worktree, &["rev-parse", "HEAD"]).await.unwrap();
        let listed = commits(&repo, &settings.base_ref(1), &head).await;
        let subjects: Vec<_> = listed.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, ["agent work"]);
    }

    /// `--fork-point` would not cover this; plain `merge-base` does.
    #[tokio::test]
    async fn an_agent_that_merges_instead_of_rebasing_also_moves_the_base() {
        let (settings, repo) = scratch("merged").await;
        let worktree = settings.worktree_path(1);

        create_worktree(&settings, &repo, &worktree, "main", 1)
            .await
            .unwrap();
        commit(&worktree, "agent.txt", "work\n", "agent work").await;
        let upstream = commit(&repo, "upstream.txt", "theirs\n", "upstream work").await;

        run(&worktree, &["merge", "main", "-m", "merge main"])
            .await
            .unwrap();

        assert_eq!(
            reconcile_base(&repo, &worktree, &settings.base_ref(1), "main").await,
            Some(upstream)
        );
    }

    #[tokio::test]
    async fn a_base_branch_that_was_rewound_does_not_drag_the_base_back() {
        let (settings, repo) = scratch("rewound").await;
        let first = run(&repo, &["rev-parse", "HEAD"]).await.unwrap();
        commit(&repo, "a.txt", "two\n", "second").await;

        let started = create_worktree(&settings, &repo, &worktree_of(&settings), "main", 1)
            .await
            .unwrap();
        commit(&worktree_of(&settings), "agent.txt", "work\n", "agent work").await;
        run(&repo, &["reset", "--hard", "-q", &first])
            .await
            .unwrap();

        assert_eq!(
            reconcile_base(
                &repo,
                &worktree_of(&settings),
                &settings.base_ref(1),
                "main"
            )
            .await,
            None
        );
        assert_eq!(
            run(&repo, &["rev-parse", &settings.base_ref(1)])
                .await
                .unwrap(),
            started
        );
    }

    #[tokio::test]
    async fn a_card_whose_base_branch_has_gone_keeps_the_base_it_started_from() {
        let (settings, repo) = scratch("branch-gone").await;
        run(&repo, &["branch", "feature"]).await.unwrap();

        let started = create_worktree(&settings, &repo, &worktree_of(&settings), "feature", 1)
            .await
            .unwrap();
        // Safe to delete: `main` is what the repo has checked out.
        run(&repo, &["branch", "-D", "feature"]).await.unwrap();

        assert_eq!(
            reconcile_base(
                &repo,
                &worktree_of(&settings),
                &settings.base_ref(1),
                "feature"
            )
            .await,
            None
        );
        assert_eq!(
            run(&repo, &["rev-parse", &settings.base_ref(1)])
                .await
                .unwrap(),
            started
        );
    }

    fn worktree_of(settings: &Settings) -> PathBuf {
        settings.worktree_path(1)
    }
}
