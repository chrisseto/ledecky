use rusqlite::Connection;

use crate::models::{Card, Comment, Lane, Project, Turn};

pub fn project(conn: &Connection, id: i64) -> Option<Project> {
    conn.query_row(
        &format!("SELECT {} FROM projects WHERE id = ?1", Project::COLUMNS),
        [id],
        Project::from_row,
    )
    .ok()
}

pub fn projects(conn: &Connection) -> Vec<Project> {
    conn.prepare(&format!(
        "SELECT {} FROM projects ORDER BY name",
        Project::COLUMNS
    ))
    .and_then(|mut s| {
        s.query_map([], Project::from_row)
            .map(|rows| rows.filter_map(Result::ok).collect())
    })
    .unwrap_or_default()
}

pub fn card(conn: &Connection, id: i64) -> Option<Card> {
    conn.query_row(
        &format!("SELECT {} FROM cards WHERE id = ?1", Card::COLUMNS),
        [id],
        Card::from_row,
    )
    .ok()
}

pub fn cards_for_project(conn: &Connection, project_id: i64) -> Vec<Card> {
    conn.prepare(&format!(
        "SELECT {} FROM cards WHERE project_id = ?1 ORDER BY position",
        Card::COLUMNS
    ))
    .and_then(|mut s| {
        s.query_map([project_id], Card::from_row)
            .map(|rows| rows.filter_map(Result::ok).collect())
    })
    .unwrap_or_default()
}

pub fn set_lane(conn: &Connection, id: i64, lane: Lane) {
    let _ = conn.execute(
        "UPDATE cards SET lane = ?1, updated_at = datetime('now') WHERE id = ?2",
        rusqlite::params![lane, id],
    );
}

pub fn turns(conn: &Connection, card_id: i64) -> Vec<Turn> {
    conn.prepare(&format!(
        "SELECT {} FROM turns WHERE card_id = ?1 ORDER BY n",
        Turn::COLUMNS
    ))
    .and_then(|mut s| {
        s.query_map([card_id], Turn::from_row)
            .map(|rows| rows.filter_map(Result::ok).collect())
    })
    .unwrap_or_default()
}

pub fn comments(conn: &Connection, card_id: i64) -> Vec<Comment> {
    conn.prepare(&format!(
        "SELECT {} FROM comments WHERE card_id = ?1 ORDER BY id",
        Comment::COLUMNS
    ))
    .and_then(|mut s| {
        s.query_map([card_id], Comment::from_row)
            .map(|rows| rows.filter_map(Result::ok).collect())
    })
    .unwrap_or_default()
}
