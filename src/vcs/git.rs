use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Result};
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

use crate::config::Settings;
use crate::vcs::Worktree;

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

/// Runs a git command against `work` and returns trimmed stdout.
///
/// NB: with `--git-dir` given and no `--work-tree`, git reads the current
/// directory as the top of the work tree. So `-C` keeps doing the work for both
/// kinds of checkout and a relative pathspec resolves the same way either side.
fn command(work: &Worktree) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(work.path());
    if let Some(git_dir) = work.git_dir() {
        cmd.arg(format!("--git-dir={}", git_dir.display()));
    }
    cmd
}

async fn run_work(work: &Worktree, args: &[&str], envs: &[(&str, &str)]) -> Result<String> {
    let mut cmd = command(work);
    cmd.args(args);
    for (key, value) in envs {
        cmd.env(key, value);
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

/// Which of `paths` — absolute, files or directories — the work tree's own rules
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
pub async fn ignored(work: &Worktree, paths: &[PathBuf]) -> HashSet<PathBuf> {
    if paths.is_empty() {
        return HashSet::new();
    }

    let mut child = match command(work)
        .args(["check-ignore", "-z", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            warn!("check-ignore in {}: {err}", work.path().display());
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
            work.path().display(),
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

/// Whether every one of `paths` is ignored by the work tree's own rules — the
/// burst filter, asked once per settled burst by the worktree watcher.
///
/// What a build writes is not work anyone is reviewing, and it arrives in the
/// thousands — so a burst that is entirely `target/` or `node_modules/` should
/// not cost a restage, let alone one per reader.
///
/// Nothing at all is not a build, and answering "yes" would swallow the burst.
pub async fn all_ignored(work: &Worktree, paths: &[PathBuf]) -> bool {
    if paths.is_empty() {
        return false;
    }

    let ignored = ignored(work, paths).await;
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

/// Every remote the repository knows, in the order git lists them.
pub async fn remotes(repo: &Path) -> Vec<String> {
    run(repo, &["remote"])
        .await
        .map(|s| s.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// One remote's branches, named the way git names them: `<remote>/<branch>`.
///
/// `<remote>/HEAD` is left out. It is a symbolic ref at whichever branch the
/// remote calls default, so offering it is the same commit twice under two
/// names — and the picker is a list of things to tell apart.
pub async fn remote_branches(repo: &Path, remote: &str) -> Vec<String> {
    let head = format!("{remote}/HEAD");
    run(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            &format!("refs/remotes/{remote}"),
        ],
    )
    .await
    .map(|s| {
        s.lines()
            .filter(|n| *n != head)
            .map(str::to_owned)
            .collect()
    })
    .unwrap_or_default()
}

/// What [`rev_parse`] is being asked to resolve.
///
/// `Head` is named rather than spelt because it is per-worktree: which
/// repository it is asked in is the whole of what it means.
#[derive(Clone, Copy, Debug)]
pub enum Rev<'a> {
    /// A revision as git reads it — `refs/{APP_SLUG}/<card>/base`, a branch, a
    /// tag, a sha, `HEAD~3`. Git reads a bare name generously, and this is that
    /// reading; [`resolve`] is where input someone typed goes.
    Ref(&'a str),
    /// Whatever the repository or worktree has checked out.
    Head,
}

impl Rev<'_> {
    /// The revision as git should read it.
    fn spec(&self) -> String {
        match self {
            Rev::Ref(name) => (*name).to_owned(),
            Rev::Head => "HEAD".to_owned(),
        }
    }
}

/// The commit a revision names, or `None` if it does not name one.
pub async fn rev_parse(repo: &Path, rev: Rev<'_>) -> Option<String> {
    run(
        repo,
        &[
            "rev-parse",
            "--verify",
            &format!("{}^{{commit}}", rev.spec()),
        ],
    )
    .await
    .ok()
    .filter(|sha| !sha.is_empty())
}

/// What a revision turned out to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefKind {
    Branch,
    Remote,
    Tag,
    /// A revision that names no ref: a sha, or an expression like `HEAD~3`.
    Commit,
}

/// A revision that resolved, and what kind of thing it named.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub sha: String,
    pub kind: RefKind,
}

/// Reads `rev` as generously as git does, and says what it was.
///
/// [`rev_parse`] answers "what commit is this?". This answers "can a card be
/// based on what someone typed?" — and says what it turned out to be, which is
/// the difference between a picker that validates and one that only accepts.
/// Everything downstream of a card's base hands it to git as a bare revision,
/// so whatever resolves here is usable.
pub async fn resolve(repo: &Path, rev: &str) -> Option<Resolved> {
    // NB: this is form input. `--verify` stops git guessing at a name, but not
    // git reading a leading dash as a flag of its own — and an empty revision
    // is `HEAD`, which would make "nothing typed" resolve.
    if rev.is_empty() || rev.starts_with('-') {
        return None;
    }

    let sha = rev_parse(repo, Rev::Ref(rev)).await?;

    // Already resolved, so a name that is not a ref is a revision rather than a
    // failure: `--symbolic-full-name` prints nothing for a sha.
    let full = run(repo, &["rev-parse", "--symbolic-full-name", rev])
        .await
        .unwrap_or_default();

    let kind = if full.starts_with("refs/heads/") {
        RefKind::Branch
    } else if full.starts_with("refs/remotes/") {
        RefKind::Remote
    } else if full.starts_with("refs/tags/") {
        RefKind::Tag
    } else {
        RefKind::Commit
    };

    Some(Resolved { sha, kind })
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
/// which is the right answer for every caller: a guard that refuses to move
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

/// Where `head` branches from `base_branch` now, or `None` when the two share
/// no history.
///
/// Always an ancestor of `head`, which is what makes a range starting here well
/// formed however the base ref got where it is. Plain `merge-base` rather than
/// `--fork-point` so that a workspace which merged instead of rebasing is read
/// the same way.
///
/// NB: `head` is passed in rather than read here. A git worktree answers it with
/// `rev-parse HEAD`, a jj workspace with `jj log -r @-`, and only the caller
/// knows which it has — see `vcs::head`.
pub async fn fork_point(repo: &Path, head: &str, base_branch: &str) -> Option<String> {
    // In the repo, where the base branch lives. The object database and
    // `refs/heads` are shared with every workspace, so this resolves either way.
    run(repo, &["merge-base", base_branch, head])
        .await
        .ok()
        .filter(|sha| !sha.is_empty())
}

/// Moves `base_ref` to wherever `head` branches from `base_branch` now.
///
/// A detached workspace does not contain new upstream commits, so a branch that
/// merely moves ahead is harmless and this does nothing. What it is for is a
/// rebase: afterwards the workspace is rooted at a commit `base_ref` has never
/// heard of, and `base_ref..head` would fold every upstream commit into the
/// range.
///
/// The ref moves *forward* only, except when it has fallen out of the workspace's
/// history entirely — see the guard. A rewound branch, or a workspace sitting at
/// an older commit, must not drag it back.
///
/// Returns the new value when it moved, and `None` otherwise — which is both the
/// ordinary case and what every failure reads as. Best-effort on purpose:
/// callers poll this, and an unresolvable `base_branch` still has to render.
pub async fn reconcile_base(
    repo: &Path,
    head: &str,
    base_ref: &str,
    base_branch: &str,
) -> Option<String> {
    let current = rev_parse(repo, Rev::Ref(base_ref)).await?;
    let candidate = fork_point(repo, head, base_branch).await?;

    if candidate == current {
        return None;
    }

    // NB: the two cases one direction cannot tell apart. While the base is still
    // behind the workspace it is a true description of where that workspace is
    // rooted, and a branch rewound under it must not drag it back. Once it is
    // not, the workspace has been rebased off the base entirely — `base..head`
    // has stopped naming a range, and no later poll could repair it — so the
    // fork point is followed wherever it went. The second question is only
    // asked once the first has already said no.
    if !is_ancestor(repo, &current, &candidate).await && is_ancestor(repo, &current, head).await {
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
async fn stage_tree(work: &Worktree, index: &Path) -> Result<String> {
    std::fs::create_dir_all(index.parent().unwrap())?;

    let index_env: &[(&str, &str)] = &[("GIT_INDEX_FILE", &index.to_string_lossy())];
    run_work(work, &["add", "-A"], index_env).await?;
    run_work(work, &["write-tree"], index_env).await
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
    work: &Worktree,
    card_id: i64,
) -> Result<String> {
    // NB: this index is deliberately *not* removed afterwards, unlike the turn
    // snapshot's. It is rewritten on every poll, and git's stat cache is the
    // only thing keeping `add -A` off a full re-hash of the tree each time.
    let index = settings.card_dir(card_id).join("working.index");
    let tree = stage_tree(work, &index).await?;

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
    /// The whole message, laid out as it was written — so a line number in a
    /// review is one the agent can find. `subject` is a one-line rendering of
    /// it and joins a first paragraph that wraps, which is why both are here.
    pub message: String,
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
    // NB: `-z` and unit separators because `%b` spans lines.
    let Ok(out) = run(
        repo,
        &[
            "log",
            "-z",
            "--no-decorate",
            "--format=%H%x1f%P%x1f%ct%x1f%s%x1f%B",
            &range,
        ],
    )
    .await
    else {
        return Vec::new();
    };

    out.split('\0')
        .take(MAX_COMMITS)
        .filter_map(|record| {
            let mut fields = record.split('\x1f');
            let sha = fields.next()?.trim().to_owned();
            // `%P` is every parent, space separated; the first is the one a
            // diff of this commit alone is measured against.
            let parent = fields.next()?.split_whitespace().next().map(str::to_owned);
            let at = fields.next()?.parse().ok()?;
            // Both are there whatever the message says: git writes the
            // separator for an empty field too.
            let subject = fields.next()?.to_owned();
            let message = fields.next()?.trim_end().to_owned();

            Some(Commit {
                sha,
                parent,
                subject,
                message,
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
    work: &Worktree,
    card_id: i64,
    n: i64,
    parent: &str,
) -> Result<Option<String>> {
    let index = settings.card_dir(card_id).join("snapshot.index");
    let _ = std::fs::remove_file(&index);
    let tree = stage_tree(work, &index).await?;
    let _ = std::fs::remove_file(&index);

    // NB: in the repository, not the work tree. Both of these are pure object
    // operations, and a jj workspace has no `.git` to ask.
    let parent_tree = tree_of(repo, parent).await;
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
        repo,
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

/// Where a worktree's commits have got to.
///
/// A detached worktree's `HEAD` is the only place its own commits are reachable
/// from — the turn refs are a parallel chain and never contain them.
pub async fn head(worktree: &Path) -> Option<String> {
    rev_parse(worktree, Rev::Head).await
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

        assert!(ignored(
            &Worktree::Git(repo.clone()),
            std::slice::from_ref(&artefact)
        )
        .await
        .contains(&artefact));

        // One tracked file in the burst is enough to make it real.
        assert!(
            !all_ignored(
                &Worktree::Git(repo.clone()),
                &[artefact, repo.join("a.txt")]
            )
            .await
        );

        // Nothing at all is not a build; saying so would swallow the burst.
        assert!(!all_ignored(&Worktree::Git(repo.clone()), &[]).await);
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

        assert!(
            ignored(&Worktree::Git(repo.clone()), &[repo.join("target")])
                .await
                .is_empty()
        );
        assert!(
            !all_ignored(
                &Worktree::Git(repo.clone()),
                &[repo.join("target/kept.txt")]
            )
            .await
        );
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
            ignored(&Worktree::Git(repo.clone()), &paths).await,
            HashSet::from([artefact.clone()])
        );
        assert!(ignored(&Worktree::Git(repo.clone()), &[]).await.is_empty());

        // A path outside the repository stops check-ignore where it stands.
        // Whatever is left unanswered has to read as work rather than as build
        // output, or a burst carrying one would be swallowed whole.
        assert!(
            !all_ignored(
                &Worktree::Git(repo.clone()),
                &[PathBuf::from("/etc/hosts"), artefact]
            )
            .await
        );
    }

    /// The premise the diff cache and every `304` rest on: an unchanged
    /// worktree has to produce the same object id every time.
    #[tokio::test]
    async fn staging_an_unchanged_worktree_is_the_same_tree_twice() {
        let (settings, repo) = scratch("stable").await;

        let once = working_tree(&settings, &repo, &Worktree::Git(repo.clone()), 1)
            .await
            .unwrap();
        let twice = working_tree(&settings, &repo, &Worktree::Git(repo.clone()), 1)
            .await
            .unwrap();
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

        let clean = working_tree(&settings, &repo, &Worktree::Git(repo.clone()), 1)
            .await
            .unwrap();

        // Both an edit and a wholly new file, which `git diff HEAD` alone would
        // miss — this is what the review pane could not show before.
        std::fs::write(repo.join("a.txt"), "two\n").unwrap();
        std::fs::write(repo.join("b.txt"), "new\n").unwrap();
        let dirty = working_tree(&settings, &repo, &Worktree::Git(repo.clone()), 1)
            .await
            .unwrap();

        assert_ne!(clean, dirty);

        let (added, removed) = diff_stat(&repo, "HEAD", &dirty).await.unwrap();
        assert_eq!((added, removed), (2, 1));
    }

    #[tokio::test]
    async fn the_scratch_index_stays_out_of_the_worktree() {
        let (settings, repo) = scratch("index").await;

        let tree = working_tree(&settings, &repo, &Worktree::Git(repo.clone()), 1)
            .await
            .unwrap();

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
            reconcile_base(
                &repo,
                &head_of(&worktree).await,
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

        run(&worktree, &["rebase", "main"]).await.unwrap();

        assert_eq!(
            reconcile_base(
                &repo,
                &head_of(&worktree).await,
                &settings.base_ref(1),
                "main"
            )
            .await,
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
            reconcile_base(
                &repo,
                &head_of(&worktree).await,
                &settings.base_ref(1),
                "main"
            )
            .await,
            None
        );

        // What the whole change is for — the upstream commit is out of the range.
        let head = run(&worktree, &["rev-parse", "HEAD"]).await.unwrap();
        let listed = commits(&repo, &settings.base_ref(1), &head).await;
        let subjects: Vec<_> = listed.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, ["agent work"]);
    }

    #[tokio::test]
    async fn resolve_reads_a_revision_and_says_what_it_was() {
        let (settings, repo) = scratch("rev-parse").await;
        let head = run(&repo, &["rev-parse", "HEAD"]).await.unwrap();
        run(&repo, &["tag", "v1"]).await.unwrap();
        run(&repo, &["update-ref", "refs/remotes/origin/main", &head])
            .await
            .unwrap();

        let kind = async |rev: &str| resolve(&repo, rev).await.map(|r| (r.sha, r.kind));
        let at = |k| Some((head.clone(), k));

        assert_eq!(kind("main").await, at(RefKind::Branch));
        assert_eq!(kind("origin/main").await, at(RefKind::Remote));
        assert_eq!(kind("v1").await, at(RefKind::Tag));
        assert_eq!(kind(&head).await, at(RefKind::Commit));
        // `HEAD` is symbolic here, so it reads as the branch it is at — which
        // is what it is, and what the chip should say about it.
        assert_eq!(kind("HEAD").await, at(RefKind::Branch));
        assert_eq!(kind("nope").await, None);

        // NB: both would otherwise be answered by git rather than refused —
        // the empty revision is `HEAD`, and a leading dash is a flag.
        assert_eq!(kind("").await, None);
        assert_eq!(kind("-C").await, None);

        // A ref of ours, by its full name, is still the narrow question.
        let base = settings.base_ref(1);
        assert_eq!(rev_parse(&repo, Rev::Ref(&base)).await, None);
        assert_eq!(rev_parse(&repo, Rev::Head).await, Some(head.clone()));
        run(&repo, &["update-ref", &base, &head]).await.unwrap();
        assert_eq!(rev_parse(&repo, Rev::Ref(&base)).await, Some(head));
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
            reconcile_base(
                &repo,
                &head_of(&worktree).await,
                &settings.base_ref(1),
                "main"
            )
            .await,
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
                &head_of(&worktree_of(&settings)).await,
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
    async fn fork_point_is_where_a_worktree_branches_from_a_branch() {
        let (settings, repo) = scratch("fork-point").await;
        let worktree = settings.worktree_path(1);

        let started = create_worktree(&settings, &repo, &worktree, "main", 1)
            .await
            .unwrap();
        commit(&worktree, "agent.txt", "work\n", "agent work").await;
        commit(&repo, "upstream.txt", "theirs\n", "upstream work").await;

        // The worktree is detached and does not contain the upstream commit, so
        // where it branches from is still where it started.
        assert_eq!(
            fork_point(&repo, &head_of(&worktree).await, "main").await,
            Some(started.clone())
        );

        // Whatever it answers is reachable from the worktree, which is what
        // makes a range starting there well formed.
        let head = run(&worktree, &["rev-parse", "HEAD"]).await.unwrap();
        assert!(is_ancestor(&repo, &started, &head).await);
    }

    /// What the refusal in `set_base` rests on: there is no range to offer.
    #[tokio::test]
    async fn a_branch_sharing_no_history_has_no_fork_point() {
        let (settings, repo) = scratch("unrelated").await;
        let worktree = settings.worktree_path(1);
        create_worktree(&settings, &repo, &worktree, "main", 1)
            .await
            .unwrap();

        // A root commit of its own: same object database, no common ancestor.
        let tree = run(&repo, &["hash-object", "-t", "tree", "/dev/null"])
            .await
            .unwrap();
        let orphan = run(&repo, &["commit-tree", &tree, "-m", "unrelated"])
            .await
            .unwrap();
        run(&repo, &["branch", "orphan", &orphan]).await.unwrap();

        assert_eq!(
            fork_point(&repo, &head_of(&worktree).await, "orphan").await,
            None
        );
        assert_eq!(
            reconcile_base(
                &repo,
                &head_of(&worktree).await,
                &settings.base_ref(1),
                "orphan"
            )
            .await,
            None
        );
    }

    /// The wedge the forward-only rule used to leave: once a rebase puts the
    /// worktree somewhere the base is not an ancestor of, `base..head` stops
    /// naming a range and no later poll could have repaired it.
    #[tokio::test]
    async fn a_worktree_rebased_off_the_base_re_derives_it() {
        let (settings, repo) = scratch("rebased-off").await;
        let worktree = settings.worktree_path(1);

        // `side` forks before the commit the card is cut from and goes its own
        // way, so the base starts on a branch of history the worktree is about
        // to leave and the move below has to go somewhere that is not ahead of
        // it.
        let forked = run(&repo, &["rev-parse", "HEAD"]).await.unwrap();
        run(&repo, &["branch", "side", &forked]).await.unwrap();
        let started = commit(&repo, "a.txt", "two\n", "second").await;

        assert_eq!(
            create_worktree(&settings, &repo, &worktree, "main", 1)
                .await
                .unwrap(),
            started
        );
        commit(&worktree, "agent.txt", "work\n", "agent work").await;

        run(&repo, &["checkout", "-q", "side"]).await.unwrap();
        let sidework = commit(&repo, "side.txt", "theirs\n", "side work").await;
        run(&repo, &["checkout", "-q", "main"]).await.unwrap();

        // Only the card's own commit is replayed, onto a branch that never had
        // `started` on it.
        run(&worktree, &["rebase", "--onto", "side", &started])
            .await
            .unwrap();
        let head = run(&worktree, &["rev-parse", "HEAD"]).await.unwrap();
        // The premise: the base is no longer in the worktree's history at all,
        // and the fork point is not ahead of it either.
        assert!(!is_ancestor(&repo, &started, &head).await);
        assert!(!is_ancestor(&repo, &started, &sidework).await);

        assert_eq!(
            reconcile_base(
                &repo,
                &head_of(&worktree).await,
                &settings.base_ref(1),
                "side"
            )
            .await,
            Some(sidework.clone())
        );
        assert_eq!(
            run(&repo, &["rev-parse", &settings.base_ref(1)])
                .await
                .unwrap(),
            sidework
        );

        // And the range it leaves is the card's work, nothing else.
        let listed = commits(&repo, &settings.base_ref(1), &head).await;
        let subjects: Vec<_> = listed.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, ["agent work"]);
    }

    /// The same repair when the branch itself was rewritten under the card,
    /// which is what a force-push looks like from here.
    #[tokio::test]
    async fn a_worktree_rebased_onto_a_rewritten_upstream_follows_it() {
        let (settings, repo) = scratch("force-pushed").await;
        let worktree = settings.worktree_path(1);

        let started = create_worktree(&settings, &repo, &worktree, "main", 1)
            .await
            .unwrap();
        commit(&worktree, "agent.txt", "work\n", "agent work").await;

        // Upstream lands something, the card rebases onto it, then upstream
        // rewrites that commit and the card rebases again.
        commit(&repo, "upstream.txt", "theirs\n", "upstream work").await;
        run(&worktree, &["rebase", "main"]).await.unwrap();
        reconcile_base(
            &repo,
            &head_of(&worktree).await,
            &settings.base_ref(1),
            "main",
        )
        .await;

        run(&repo, &["reset", "--hard", "-q", &started])
            .await
            .unwrap();
        let rewritten = commit(&repo, "upstream.txt", "theirs, again\n", "upstream work").await;
        run(&worktree, &["rebase", "--onto", "main", "HEAD~1"])
            .await
            .unwrap();

        assert_eq!(
            reconcile_base(
                &repo,
                &head_of(&worktree).await,
                &settings.base_ref(1),
                "main"
            )
            .await,
            Some(rewritten.clone())
        );

        let head = run(&worktree, &["rev-parse", "HEAD"]).await.unwrap();
        let listed = commits(&repo, &settings.base_ref(1), &head).await;
        let subjects: Vec<_> = listed.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, ["agent work"]);
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
                &head_of(&worktree_of(&settings)).await,
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

    /// What `vcs::head` does for a git workspace, which `fork_point` and
    /// `reconcile_base` now take as an argument rather than reading themselves.
    async fn head_of(worktree: &Path) -> String {
        rev_parse(worktree, Rev::Head).await.unwrap_or_default()
    }

    fn worktree_of(settings: &Settings) -> PathBuf {
        settings.worktree_path(1)
    }

    /// A review comment names a line of the message, so the lines have to be
    /// the ones git wrote — `%s` joins a first paragraph that wraps.
    #[tokio::test]
    async fn a_commit_carries_its_message_as_it_was_written() {
        let (_settings, repo) = scratch("messages").await;
        let base = run(&repo, &["rev-parse", "HEAD"]).await.unwrap();

        std::fs::write(repo.join("a.txt"), "two\n").unwrap();
        run(&repo, &["commit", "-qam", "banner: land it\nand wrap"])
            .await
            .unwrap();
        std::fs::write(repo.join("a.txt"), "three\n").unwrap();
        run(
            &repo,
            &[
                "commit",
                "-qam",
                "banner: explain it\n\nWhy it prints first.",
            ],
        )
        .await
        .unwrap();

        let listed = commits(&repo, &base, "HEAD").await;
        let messages: Vec<_> = listed.iter().map(|c| c.message.as_str()).collect();
        assert_eq!(
            messages,
            [
                "banner: explain it\n\nWhy it prints first.",
                "banner: land it\nand wrap"
            ]
        );
        // The picker's label is still the one-line rendering.
        assert_eq!(listed[1].subject, "banner: land it and wrap");

        // A commit with no message at all is still a commit, and git writes
        // the empty fields rather than leaving them out.
        std::fs::write(repo.join("a.txt"), "four\n").unwrap();
        run(&repo, &["commit", "-qam", "", "--allow-empty-message"])
            .await
            .unwrap();
        let with_empty = commits(&repo, &base, "HEAD").await;
        assert_eq!(with_empty.len(), 3);
        assert_eq!(with_empty[0].message, "");
    }
}
