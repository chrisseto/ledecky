use minijinja::{AutoEscape, Environment, UndefinedBehavior};
use rocket::request::FromParam;
use rocket::serde::Serialize;
use sqlx::sqlite::SqliteRow;
use sqlx::{FromRow, Row};

use crate::db::{sql, DB};
use crate::project::Project;
use crate::vcs::VCS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(crate = "rocket::serde", rename_all = "lowercase")]
pub enum Kind {
    /// Rendered once into the task of a new card.
    Template,
    /// Rendered and sent to the agent of a card in review.
    Action,
}

impl Kind {
    pub const ALL: &'static [Self] = &[Self::Template, Self::Action];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Template => "template",
            Self::Action => "action",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Template => "Templates",
            Self::Action => "Actions",
        }
    }

    /// The URL segment.
    pub fn plural(self) -> &'static str {
        match self {
            Self::Template => "templates",
            Self::Action => "actions",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|kind| kind.as_str() == raw)
    }
}

impl<'a> FromParam<'a> for Kind {
    type Error = &'a str;

    fn from_param(param: &'a str) -> Result<Self, Self::Error> {
        Self::ALL
            .iter()
            .copied()
            .find(|kind| kind.plural() == param)
            .ok_or(param)
    }
}

/// A Jinja template the user writes in the settings modal.
#[derive(Debug, Clone, Serialize)]
#[serde(crate = "rocket::serde")]
pub struct Prompt {
    pub id: i64,
    pub kind: Kind,
    pub name: String,
    pub body: String,
    /// Move the card to Done when its work reaches the base. Actions only.
    pub lands: bool,
}

impl Prompt {
    const COLUMNS: &'static str = "id, kind, name, body, lands";

    /// In the order they were made.
    pub async fn all(db: &DB, kind: Kind) -> sqlx::Result<Vec<Self>> {
        sqlx::query_as(sql(format!(
            "SELECT {} FROM prompts WHERE kind = ?1 ORDER BY id",
            Self::COLUMNS
        )))
        .bind(kind.as_str())
        .fetch_all(db.pool())
        .await
    }

    pub async fn find(db: &DB, kind: Kind, id: i64) -> sqlx::Result<Option<Self>> {
        sqlx::query_as(sql(format!(
            "SELECT {} FROM prompts WHERE id = ?1 AND kind = ?2",
            Self::COLUMNS
        )))
        .bind(id)
        .bind(kind.as_str())
        .fetch_optional(db.pool())
        .await
    }

    pub async fn create(db: &DB, kind: Kind) -> sqlx::Result<i64> {
        Ok(
            sqlx::query("INSERT INTO prompts (kind, name) VALUES (?1, 'Untitled')")
                .bind(kind.as_str())
                .execute(db.pool())
                .await?
                .last_insert_rowid(),
        )
    }

    /// Whether a prompt of `kind` had that id.
    pub async fn update(
        db: &DB,
        kind: Kind,
        id: i64,
        name: &str,
        body: &str,
        lands: bool,
    ) -> sqlx::Result<bool> {
        let done = sqlx::query(
            "UPDATE prompts SET name = ?3, body = ?4, lands = ?5 WHERE id = ?1 AND kind = ?2",
        )
        .bind(id)
        .bind(kind.as_str())
        .bind(name)
        .bind(body)
        .bind(lands && kind == Kind::Action)
        .execute(db.pool())
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Whether a prompt of `kind` had that id.
    pub async fn delete(db: &DB, kind: Kind, id: i64) -> sqlx::Result<bool> {
        let done = sqlx::query("DELETE FROM prompts WHERE id = ?1 AND kind = ?2")
            .bind(id)
            .bind(kind.as_str())
            .execute(db.pool())
            .await?;
        Ok(done.rows_affected() > 0)
    }
}

impl<'r> FromRow<'r, SqliteRow> for Prompt {
    fn from_row(row: &'r SqliteRow) -> sqlx::Result<Self> {
        let kind: String = row.try_get("kind")?;
        Ok(Self {
            id: row.try_get("id")?,
            kind: Kind::parse(&kind)
                .ok_or_else(|| sqlx::Error::Decode(format!("unknown prompt kind {kind}").into()))?,
            name: row.try_get("name")?,
            body: row.try_get("body")?,
            lands: row.try_get("lands")?,
        })
    }
}

/// What a prompt can refer to. `_modal_settings.html` lists these names;
/// change both together.
#[derive(Serialize)]
#[serde(crate = "rocket::serde")]
pub struct Vars<'a> {
    pub task: &'a str,
    pub branch: &'a str,
    pub repo: &'a str,
    pub project: &'a str,
    pub vcs: &'static str,
    pub git: bool,
    pub jujutsu: bool,
}

impl<'a> Vars<'a> {
    pub fn new(project: &'a Project, task: &'a str, branch: &'a str, vcs: VCS) -> Self {
        Self {
            task,
            branch,
            repo: &project.path,
            project: &project.name,
            vcs: vcs.as_str(),
            git: vcs == VCS::Git,
            jujutsu: vcs == VCS::JJ,
        }
    }
}

pub fn render(body: &str, vars: &Vars) -> Result<String, minijinja::Error> {
    environment().render_str(body, vars)
}

const NAMES: &[&str] = &["task", "branch", "repo", "project", "vcs", "git", "jujutsu"];

/// Fails on a syntax error or a name that [`Vars`] does not have.
pub fn check(body: &str) -> Result<(), String> {
    let env = environment();
    let template = env.template_from_str(body).map_err(|err| err.to_string())?;
    // NB: this finds a misspelled name in a branch that a render does not take.
    let mut unknown: Vec<_> = template
        .undeclared_variables(false)
        .into_iter()
        .filter(|name| !NAMES.contains(&name.as_str()) && env.globals().all(|(g, _)| g != name))
        .collect();
    unknown.sort();
    match unknown.is_empty() {
        true => Ok(()),
        false => Err(format!("Unknown variable: {}", unknown.join(", "))),
    }
}

fn environment() -> Environment<'static> {
    let mut env = Environment::new();
    // NB: strict, so a misspelled variable is an error, not an empty string.
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    // NB: no HTML escape. The output goes to an agent, not to a page.
    env.set_auto_escape_callback(|_| AutoEscape::None);
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::memory_db;

    fn project(path: &str) -> Project {
        Project {
            id: 1,
            name: "demo".into(),
            path: path.into(),
            created_at: String::new(),
        }
    }

    async fn merge(db: &DB) -> Prompt {
        Prompt::all(db, Kind::Action)
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.name == "Merge")
            .unwrap()
    }

    #[tokio::test]
    async fn the_seeded_merge_points_git_at_the_main_checkout() {
        let db = memory_db().await;
        let merge = merge(&db).await;
        assert!(merge.lands);

        let project = project("/srv/repo");
        let prompt = render(&merge.body, &Vars::new(&project, "", "main", VCS::Git)).unwrap();

        assert!(prompt.starts_with("The reviewer approved this work. Land it on `main`:"));
        // Without the main repository path, the agent runs `git branch -f` in
        // the worktree and git refuses it.
        assert!(prompt.contains("checked out in the main repository at `/srv/repo`"));
        assert!(prompt.contains("git -C /srv/repo merge --ff-only"));
        assert!(prompt.contains("`main`:\n\n1. Commit anything"));
        assert!(prompt.ends_with("SHA of `main`.\n\nDo not push."));
        assert!(!prompt.contains("jj "));
    }

    /// The jj prompt must send the agent to the main repository. Otherwise the
    /// bookmark does not reach `refs/heads` and the server does not see the
    /// merge complete.
    #[tokio::test]
    async fn the_seeded_merge_moves_the_jj_bookmark_at_the_root() {
        let db = memory_db().await;
        let project = project("/srv/project");
        let prompt = render(
            &merge(&db).await.body,
            &Vars::new(&project, "", "main", VCS::JJ),
        )
        .unwrap();

        assert!(prompt.contains("jj -R /srv/project bookmark set main"));
        assert!(prompt.contains("jj commit"));
        // Git's refusal to move a checked-out branch is not a rule jj has.
        assert!(!prompt.contains("cannot be"));
    }

    #[tokio::test]
    async fn a_prompt_round_trips() {
        let db = memory_db().await;
        let id = Prompt::create(&db, Kind::Template).await.unwrap();
        let updated = Prompt::update(&db, Kind::Template, id, "Bug", "Fix {{ task }}", true);
        assert!(updated.await.unwrap());

        // The other kind does not reach it.
        assert!(!Prompt::update(&db, Kind::Action, id, "x", "x", true)
            .await
            .unwrap());
        assert!(!Prompt::delete(&db, Kind::Action, id).await.unwrap());
        assert!(Prompt::find(&db, Kind::Action, id).await.unwrap().is_none());

        let found = Prompt::find(&db, Kind::Template, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (found.name.as_str(), found.body.as_str()),
            ("Bug", "Fix {{ task }}")
        );
        assert!(!found.lands, "only an action lands");

        assert!(Prompt::delete(&db, Kind::Template, id).await.unwrap());
        assert!(Prompt::find(&db, Kind::Template, id)
            .await
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_misspelled_variable_is_an_error() {
        let project = project("/srv/repo");
        let vars = Vars::new(&project, "x", "main", VCS::Git);
        assert!(render("{{ tsak }}", &vars).is_err());
        assert_eq!(render("<{{ task }}>", &vars).unwrap(), "<x>");
        assert!(check("{% if %}").is_err());
        assert_eq!(
            check("{% if jujutsu %}{{ tsak }}{% endif %}"),
            Err("Unknown variable: tsak".into())
        );
        assert_eq!(
            check("{% set x = task %}{% for i in range(2) %}{{ x }}{{ loop.index }}{% endfor %}"),
            Ok(())
        );
    }
}
