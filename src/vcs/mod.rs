//! Which VCS makes a card's workspace, and the dispatch between them.
//!
//! [`git`] owns every git primitive and [`jj`] every jj one; neither imports the
//! other. This module owns the choosing — two arms over a handful of
//! operations, which is why there is no trait here.
//!
//! jj is only offered for a *colocated* project: one with both `.jj` and `.git`
//! at its root. That is the whole reason the rest of the app is untouched —
//! the object store, `refs/heads` and the project's `.git` stay where they are,
//! so every read that resolves an object keeps running against the repository.
//! Only the checkout differs, and a jj workspace has none of its own.

pub mod git;
mod jj;

use std::path::{Path, PathBuf};

use anyhow::Result;
use rocket::serde::Serialize;

use crate::config::Settings;

/// What made a card's workspace.
///
/// Recorded per card and frozen once a session exists: it describes the
/// workspace on disk rather than a preference, so it is never re-derived from
/// the project afterwards.
/// NB: `lowercase`, not `snake_case`. Snake case inserts a separator before
/// every non-leading capital, so `JJ` would serialize as `"j_j"` while
/// [`VCS::as_str`] and the column both say `"jj"` — a template comparing
/// `card.vcs` against either spelling would silently take the other arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(crate = "rocket::serde", rename_all = "lowercase")]
pub enum VCS {
    Git,
    JJ,
}

impl VCS {
    pub const ALL: &'static [Self] = &[Self::Git, Self::JJ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::JJ => "jj",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Git => "Git",
            Self::JJ => "Jujutsu",
        }
    }

    pub fn parse(raw: &str) -> Self {
        Self::ALL
            .iter()
            .copied()
            .find(|vcs| vcs.as_str() == raw)
            .unwrap_or(Self::Git)
    }

    /// What marks a workspace of this kind on disk.
    fn marker(self) -> &'static str {
        match self {
            Self::Git => ".git",
            Self::JJ => ".jj",
        }
    }
}

/// A card's checkout, and what git needs to be told to read it.
///
/// A git worktree carries its own `.git` and so answers for itself. A jj
/// workspace does not: its objects are the project's, which is the one thing
/// anything outside this module has to know about it.
#[derive(Debug, Clone)]
pub enum Worktree {
    Git(PathBuf),
    JJ { path: PathBuf, repo: PathBuf },
}

impl Worktree {
    pub fn new(vcs: VCS, repo: &Path, path: &Path) -> Self {
        match vcs {
            VCS::Git => Self::Git(path.to_path_buf()),
            VCS::JJ => Self::JJ {
                path: path.to_path_buf(),
                repo: repo.to_path_buf(),
            },
        }
    }

    /// Where the files are.
    pub fn path(&self) -> &Path {
        match self {
            Self::Git(path) => path,
            Self::JJ { path, .. } => path,
        }
    }

    /// The repository git has to be pointed at, for a checkout with no `.git`
    /// of its own.
    fn git_dir(&self) -> Option<PathBuf> {
        match self {
            Self::Git(_) => None,
            Self::JJ { repo, .. } => Some(repo.join(".git")),
        }
    }
}

/// What `repo` can make a workspace with, in the order a form should offer them.
///
/// `git` whenever there is a `.git`, and `jj` only alongside it: a jj repo that
/// is not colocated has no git checkout to stage and no `refs/heads` to read, so
/// it is not offered rather than offered and broken. Answers nothing at all for
/// a directory that is neither, which is what rejects it as a project.
///
/// NB: what is on disk, not what is installed. A colocated repo on a machine
/// with no `jj` still offers it, and the card fails when it tries to make the
/// workspace — see the error `jj::run` mints for that. Probing `PATH` on every
/// form render would buy a clearer message for a case the flake already rules
/// out, since it wraps the binary with `jj` on `PATH` beside `git` and `delta`.
///
/// Read live rather than recorded, so a repo that gains or loses `.jj` is right
/// without anything having to notice.
pub fn detect(repo: &Path) -> Vec<VCS> {
    let mut found = Vec::new();
    // `.git` is a directory in a normal clone and a file in a linked worktree.
    if repo.join(VCS::Git.marker()).exists() {
        found.push(VCS::Git);
        if repo.join(VCS::JJ.marker()).exists() {
            found.push(VCS::JJ);
        }
    }
    found
}

/// Whether `path` holds a workspace of this kind.
///
/// What tells the rest of the app a card has somewhere to work — a half-removed
/// one loses the race either way, which is the same answer as before.
pub fn exists(vcs: VCS, path: &Path) -> bool {
    path.join(vcs.marker()).exists()
}

/// Creates the card's workspace and records where its diffs start from.
///
/// Returns the base commit.
pub async fn create(
    settings: &Settings,
    vcs: VCS,
    repo: &Path,
    path: &Path,
    base_branch: &str,
    card_id: i64,
) -> Result<String> {
    // A card records what made its workspace, and that is never re-derived — not
    // here and not by the form, which only offers the choice on a new card. But
    // a repo can stop offering jj between a card being written and being
    // started: `jj git colocation disable` is all it takes.
    //
    // Refused rather than quietly made with git, which would leave the recorded
    // choice disagreeing with what is on disk and the card unable to be torn
    // down. The cost is that such a card cannot be started at all and has to be
    // deleted and remade — which is cheap for something still in To Do, and
    // louder than the alternative.
    if !detect(repo).contains(&vcs) {
        anyhow::bail!("{} cannot make a {} workspace", repo.display(), vcs.label());
    }

    match vcs {
        VCS::Git => git::create_worktree(settings, repo, path, base_branch, card_id).await,
        VCS::JJ => jj::create(settings, repo, path, base_branch, card_id).await,
    }
}

/// Tears down a card's workspace. Best-effort: a missing one is not an error.
pub async fn remove(settings: &Settings, vcs: VCS, repo: &Path, path: &Path, card_id: i64) {
    match vcs {
        VCS::Git => git::remove_worktree(repo, path).await,
        VCS::JJ => jj::remove(settings, repo, path, card_id).await,
    }
}

/// The last commit the workspace made, which is where a range of its work ends.
///
/// An agent that only edits files and never commits leaves this at the base, and
/// that is right: its work is uncommitted, and the staged tree reads the
/// filesystem rather than any head.
pub async fn head(vcs: VCS, path: &Path) -> Option<String> {
    match vcs {
        VCS::Git => git::head(path).await,
        VCS::JJ => jj::head(path).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::git::Rev;
    use rocket::figment::providers::Serialized;
    use std::path::PathBuf;

    /// What a template comparing `card.vcs` against a literal depends on, and
    /// what `snake_case` would quietly have broken: `"j_j"`.
    #[test]
    fn the_serialized_name_is_the_one_the_column_holds() {
        for vcs in VCS::ALL {
            let wire = rocket::serde::json::to_string(vcs).unwrap();
            assert_eq!(wire, format!("\"{}\"", vcs.as_str()));
        }
    }

    #[test]
    fn a_name_round_trips_and_anything_else_is_git() {
        for vcs in VCS::ALL {
            assert_eq!(VCS::parse(vcs.as_str()), *vcs);
        }
        // An unknown name is git rather than an error: the column is `NOT NULL
        // DEFAULT 'git'` and a row that somehow says otherwise still has to
        // render.
        assert_eq!(VCS::parse(""), VCS::Git);
        assert_eq!(VCS::parse("hg"), VCS::Git);
    }

    #[test]
    fn jj_is_only_offered_alongside_git() {
        let root = std::env::temp_dir().join(format!("ledecky-detect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // Neither: not a repository at all, which is what rejects it.
        assert_eq!(detect(&root), vec![]);

        // jj alone is not offered — there is no git work tree to stage and no
        // `refs/heads` to read.
        std::fs::create_dir_all(root.join(".jj")).unwrap();
        assert_eq!(detect(&root), vec![]);

        std::fs::create_dir_all(root.join(".git")).unwrap();
        assert_eq!(detect(&root), vec![VCS::Git, VCS::JJ]);

        std::fs::remove_dir_all(root.join(".jj")).unwrap();
        assert_eq!(detect(&root), vec![VCS::Git]);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A jj workspace has no `.git`, so git has to be pointed at the project's;
    /// a git worktree has one and must not be given another.
    #[test]
    fn only_a_jj_workspace_needs_the_projects_gitdir() {
        let repo = Path::new("/srv/project");
        let path = Path::new("/data/worktrees/7");

        let git_work = Worktree::new(VCS::Git, repo, path);
        assert_eq!(git_work.path(), path);
        assert_eq!(git_work.git_dir(), None);

        let jj_work = Worktree::new(VCS::JJ, repo, path);
        assert_eq!(jj_work.path(), path);
        assert_eq!(jj_work.git_dir(), Some(repo.join(".git")));
    }

    // ---- against a real jj ---------------------------------------------------
    //
    // No skip guard on `jj` being absent, matching the stance the suite already
    // takes on `git` and `delta`: the flake supplies all three, and a test that
    // quietly passes without the binary it is about is worse than one that
    // fails loudly.
    //
    // NB: deliberately *no* `JJ_CONFIG`. Setting it would mean
    // `std::env::set_var` from one test thread while others are spawning
    // children, which is a data race on the environment — the reason Rust 2024
    // makes that call unsafe, and a nondeterministic abort rather than a failed
    // assertion when it bites.
    //
    // The developer's own config is read instead, so the one setting that could
    // break these is named explicitly: `jj git init --colocate` cannot be
    // turned off by `git.colocate`, and the colocation is asserted besides. The
    // rest — templates, snapshot limits — cannot reach these tests, which name
    // their own `-T` and stage through git.

    /// A colocated jj repository with one commit, and settings pointed
    /// somewhere a card's scratch index can live.
    async fn jj_scratch(name: &str) -> (Settings, PathBuf) {
        let root = std::env::temp_dir().join(format!("ledecky-vcs-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        std::fs::create_dir_all(&root).unwrap();

        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git::run(&repo, &["init", "-q", "--initial-branch=main"])
            .await
            .unwrap();
        git::run(&repo, &["config", "user.email", "t@example.com"])
            .await
            .unwrap();
        git::run(&repo, &["config", "user.name", "t"])
            .await
            .unwrap();
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        std::fs::write(repo.join(".gitignore"), "target/\n").unwrap();
        git::run(&repo, &["add", "-A"]).await.unwrap();
        git::run(&repo, &["commit", "-qm", "first"]).await.unwrap();

        // Colocated, which is what `detect` requires and what leaves
        // `refs/heads` and the object store where every git read expects them.
        //
        // NB: `--colocate` spelled out, though it is the default for a
        // git-backed repo. The default is what `git.colocate` overrides, and
        // these tests read whoever's config is on the machine.
        //
        // NB: by working directory, not `--repository`. That flag names an
        // existing repo, which is the one thing this does not have yet — so
        // `jj::run`, which always passes it, cannot do this one.
        let init = tokio::process::Command::new("jj")
            .args(["git", "init", "--colocate"])
            .current_dir(&repo)
            .output()
            .await
            .unwrap();
        assert!(
            init.status.success(),
            "jj git init: {}",
            String::from_utf8_lossy(&init.stderr)
        );
        // The premise every assertion below rests on, said once here so a
        // machine that somehow produced a non-colocated repo fails saying so.
        assert_eq!(detect(&repo), vec![VCS::Git, VCS::JJ]);

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

    /// The premise the whole design rests on: a jj workspace has no `.git`, and
    /// is staged against the project's instead.
    #[tokio::test]
    async fn a_jj_workspace_stages_against_the_project_and_hides_its_own_state() {
        let (settings, repo) = jj_scratch("stage").await;
        assert_eq!(detect(&repo), vec![VCS::Git, VCS::JJ]);

        let path = settings.worktree_path(1);
        let base = create(&settings, VCS::JJ, &repo, &path, "main", 1)
            .await
            .unwrap();

        // What makes this not a git worktree, and so what `Worktree::JJ`
        // carrying the repository is for.
        assert!(!path.join(".git").exists());
        assert!(exists(VCS::JJ, &path));
        assert!(!exists(VCS::Git, &path));
        // The base ref landed before the workspace, as in the git arm.
        assert_eq!(
            git::rev_parse(&repo, Rev::Ref(&settings.base_ref(1))).await,
            Some(base.clone())
        );

        let work = Worktree::new(VCS::JJ, &repo, &path);
        std::fs::write(path.join("a.txt"), "two\n").unwrap();
        std::fs::write(path.join("b.txt"), "new\n").unwrap();
        // Build output, to prove the repo's own rules still apply through the
        // project's gitdir.
        std::fs::create_dir_all(path.join("target")).unwrap();
        std::fs::write(path.join("target/out.o"), "junk").unwrap();

        let once = git::working_tree(&settings, &repo, &work, 1).await.unwrap();
        let twice = git::working_tree(&settings, &repo, &work, 1).await.unwrap();
        // The premise the diff cache and every `304` rest on.
        assert_eq!(once, twice);

        let listed = git::run(&repo, &["ls-tree", "-r", "--name-only", &once])
            .await
            .unwrap();
        let paths: Vec<&str> = listed.lines().collect();
        assert_eq!(paths, [".gitignore", "a.txt", "b.txt"]);
        // jj's own bookkeeping is not work to review, and `add -A` has no
        // instinct for `.jj` the way it has for `.git`.
        assert!(!listed.contains(".jj"));
        // And the repo's rules reach the workspace through the shared gitdir.
        assert!(!listed.contains("target/"));

        // Uncommitted work is visible, which is the point of staging at all.
        assert_eq!(git::diff_stat(&repo, &base, &once).await, Some((2, 1)));
    }

    /// `@-` rather than `@`: an untouched workspace reports the base, so
    /// `base..head` is empty exactly as a detached git worktree reports it.
    #[tokio::test]
    async fn the_head_of_a_jj_workspace_is_its_last_commit() {
        let (settings, repo) = jj_scratch("head").await;
        let path = settings.worktree_path(1);
        let base = create(&settings, VCS::JJ, &repo, &path, "main", 1)
            .await
            .unwrap();

        let fresh = head(VCS::JJ, &path).await.unwrap();
        assert_eq!(fresh, base, "a fresh workspace is still at its base");
        // Full hex, because everything downstream compares it against
        // `rev-parse` output.
        assert_eq!(fresh.len(), 40);
        assert!(git::commits(&repo, &base, &fresh).await.is_empty());

        // An agent that edits but never runs jj leaves the head where it is —
        // and its work is still staged, which the test above covers.
        std::fs::write(path.join("a.txt"), "two\n").unwrap();
        assert_eq!(head(VCS::JJ, &path).await.as_deref(), Some(base.as_str()));

        // Once it commits, the head moves and the commit is the card's own work.
        jj::run(&path, &["commit", "-m", "agent work"])
            .await
            .unwrap();
        let moved = head(VCS::JJ, &path).await.unwrap();
        assert_ne!(moved, base);

        let listed = git::commits(&repo, &base, &moved).await;
        let subjects: Vec<_> = listed.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, ["agent work"]);

        // The card's own refs are not jj's to manage, and a jj command must not
        // take them: jj exports `refs/heads` and `refs/remotes` and nothing else.
        assert_eq!(
            git::rev_parse(&repo, Rev::Ref(&settings.base_ref(1))).await,
            Some(base)
        );
    }

    /// A rebase in the workspace still re-roots the card, through the same
    /// `merge-base` the git arm uses.
    #[tokio::test]
    async fn a_rebase_in_a_jj_workspace_moves_the_base() {
        let (settings, repo) = jj_scratch("rebase").await;
        let path = settings.worktree_path(1);
        let base = create(&settings, VCS::JJ, &repo, &path, "main", 1)
            .await
            .unwrap();

        std::fs::write(path.join("agent.txt"), "work\n").unwrap();
        jj::run(&path, &["commit", "-m", "agent work"])
            .await
            .unwrap();

        // Upstream lands something, and the card rebases onto it.
        std::fs::write(repo.join("upstream.txt"), "theirs\n").unwrap();
        jj::run(&repo, &["commit", "-m", "upstream work"])
            .await
            .unwrap();
        jj::run(&repo, &["bookmark", "set", "main", "-r", "@-"])
            .await
            .unwrap();
        let upstream = git::rev_parse(&repo, Rev::Ref("refs/heads/main"))
            .await
            .unwrap();
        assert_ne!(upstream, base);

        jj::run(&path, &["rebase", "-d", "main"]).await.unwrap();
        let head = head(VCS::JJ, &path).await.unwrap();

        assert_eq!(
            git::reconcile_base(&repo, &head, &settings.base_ref(1), "main").await,
            Some(upstream.clone())
        );
        // What the whole thing is for: the upstream commit is out of the range.
        let listed = git::commits(&repo, &settings.base_ref(1), &head).await;
        let subjects: Vec<_> = listed.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, ["agent work"]);
    }

    /// `@-` matches both parents of a merge working copy, and the template is
    /// rendered once per revision with nothing between them — so the naive read
    /// hands back two concatenated ids that every later `merge-base` rejects.
    #[tokio::test]
    async fn a_merge_working_copy_still_names_one_commit() {
        let (settings, repo) = jj_scratch("merge").await;
        let path = settings.worktree_path(1);
        let base = create(&settings, VCS::JJ, &repo, &path, "main", 1)
            .await
            .unwrap();

        // Two commits off the base, then a working copy on both.
        std::fs::write(path.join("a.txt"), "a\n").unwrap();
        jj::run(&path, &["commit", "-m", "side a"]).await.unwrap();
        let a = head(VCS::JJ, &path).await.unwrap();
        jj::run(&path, &["new", &base]).await.unwrap();
        std::fs::write(path.join("b.txt"), "b\n").unwrap();
        jj::run(&path, &["commit", "-m", "side b"]).await.unwrap();
        let b = head(VCS::JJ, &path).await.unwrap();
        jj::run(&path, &["new", &a, &b]).await.unwrap();

        let merged = head(VCS::JJ, &path).await.unwrap();
        assert_eq!(merged.len(), 40, "got {merged}");
        assert!(merged == a || merged == b, "{merged} is neither parent");
        // And it is a revision the rest of the app can actually use.
        assert!(git::rev_parse(&repo, Rev::Ref(&merged)).await.is_some());
    }

    #[tokio::test]
    async fn removing_a_jj_workspace_takes_the_directory_and_the_registration() {
        let (settings, repo) = jj_scratch("remove").await;
        let path = settings.worktree_path(1);
        create(&settings, VCS::JJ, &repo, &path, "main", 1)
            .await
            .unwrap();
        assert!(exists(VCS::JJ, &path));

        remove(&settings, VCS::JJ, &repo, &path, 1).await;

        assert!(!path.exists());
        // jj would otherwise keep offering it, and a stale registration is what
        // makes the next `workspace add` of the same name fail.
        let listed = jj::run(&repo, &["--ignore-working-copy", "workspace", "list"])
            .await
            .unwrap();
        assert!(
            !listed.contains(&settings.workspace_name(1)),
            "still registered: {listed}"
        );

        // Best-effort, like the git arm: doing it twice is not an error.
        remove(&settings, VCS::JJ, &repo, &path, 1).await;
    }

    /// A repo that has stopped offering jj says so, rather than failing somewhere
    /// inside jj or quietly making a git worktree the card does not describe.
    #[tokio::test]
    async fn a_repo_that_no_longer_offers_jj_refuses_plainly() {
        let (settings, repo) = jj_scratch("unoffered").await;
        // Standing in for `jj git colocation disable`, which is the real way a
        // project loses this.
        std::fs::rename(repo.join(".jj"), repo.join(".jj-away")).unwrap();
        assert_eq!(detect(&repo), vec![VCS::Git]);

        let err = create(
            &settings,
            VCS::JJ,
            &repo,
            &settings.worktree_path(1),
            "main",
            1,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("Jujutsu"), "{err}");
        assert!(!settings.worktree_path(1).exists());

        // Git is still fine, so the refusal is about the choice and not the repo.
        std::fs::rename(repo.join(".jj-away"), repo.join(".jj")).unwrap();
        create(
            &settings,
            VCS::Git,
            &repo,
            &settings.worktree_path(2),
            "main",
            2,
        )
        .await
        .unwrap();
    }

    /// A base that does not name a branch is refused before anything is made,
    /// so a failed create leaves nothing for the next `start` to misread.
    #[tokio::test]
    async fn a_jj_workspace_is_not_left_half_made() {
        let (settings, repo) = jj_scratch("partial").await;
        let path = settings.worktree_path(1);

        assert!(create(&settings, VCS::JJ, &repo, &path, "nope", 1)
            .await
            .is_err());
        assert!(!exists(VCS::JJ, &path));

        // And a second attempt at a path that already holds one is refused
        // rather than quietly adopting it.
        create(&settings, VCS::JJ, &repo, &path, "main", 1)
            .await
            .unwrap();
        assert!(create(&settings, VCS::JJ, &repo, &path, "main", 1)
            .await
            .is_err());
    }
}
