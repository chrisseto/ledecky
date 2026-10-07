-- Which VCS made the card's workspace: `git` | `jj`.
--
-- Not a preference so much as a description — it is read to decide how to stage
-- the worktree and how to tear it down, so it is frozen once a session exists
-- and never re-derived from the project afterwards.
--
-- Defaulted rather than nullable: every row written before this has a git
-- worktree, and so does every card whose project is not a colocated jj repo.
ALTER TABLE cards ADD COLUMN vcs TEXT NOT NULL DEFAULT 'git';
