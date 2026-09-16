-- Set while the agent is applying its work onto the base branch. The Stop hook
-- checks whether the branch actually moved before calling the card done.
ALTER TABLE cards ADD COLUMN merge_requested INTEGER NOT NULL DEFAULT 0;

-- Where the base branch pointed when the merge was asked for.
ALTER TABLE cards ADD COLUMN merge_base_sha TEXT;
