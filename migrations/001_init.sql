CREATE TABLE projects (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL,
    path       TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- `lane` is the kanban column: todo | in_progress | in_review | done.
-- `agent_state` tracks the claude process independently: it is possible to sit
-- in `in_review` with a still-running agent.
CREATE TABLE cards (
    id              INTEGER PRIMARY KEY,
    project_id      INTEGER NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
    title           TEXT NOT NULL,
    description     TEXT NOT NULL DEFAULT '',
    base_branch     TEXT NOT NULL,
    lane            TEXT NOT NULL DEFAULT 'todo',
    position        REAL NOT NULL DEFAULT 0,
    permission_mode TEXT NOT NULL DEFAULT 'acceptEdits',
    model           TEXT,
    worktree_path   TEXT,
    session_id      TEXT,
    agent_state     TEXT NOT NULL DEFAULT 'stopped',
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX cards_by_lane ON cards (project_id, lane, position);

CREATE TABLE turns (
    id                     INTEGER PRIMARY KEY,
    card_id                INTEGER NOT NULL REFERENCES cards (id) ON DELETE CASCADE,
    n                      INTEGER NOT NULL,
    ref_name               TEXT NOT NULL,
    commit_sha             TEXT NOT NULL,
    parent_sha             TEXT NOT NULL,
    last_assistant_message TEXT,
    created_at             TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (card_id, n)
);

CREATE TABLE comments (
    id         INTEGER PRIMARY KEY,
    card_id    INTEGER NOT NULL REFERENCES cards (id) ON DELETE CASCADE,
    turn_id    INTEGER REFERENCES turns (id) ON DELETE SET NULL,
    file_path  TEXT NOT NULL,
    line       INTEGER NOT NULL,
    side       TEXT NOT NULL DEFAULT 'new',
    body       TEXT NOT NULL,
    state      TEXT NOT NULL DEFAULT 'draft',
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX comments_by_card ON comments (card_id, state);

CREATE TABLE events (
    id           INTEGER PRIMARY KEY,
    card_id      INTEGER NOT NULL REFERENCES cards (id) ON DELETE CASCADE,
    kind         TEXT NOT NULL,
    payload_json TEXT,
    created_at   TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX events_by_card ON events (card_id, id);
