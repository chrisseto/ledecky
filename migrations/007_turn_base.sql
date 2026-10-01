-- The base ref as it stood when the snapshot was taken, so a range can tell
-- whether its two ends are either side of a rebase. `parent_sha` says this for
-- the first turn of a chain and nothing after it, since later turns parent on
-- their predecessor.
--
-- Nullable: rows written before this, and turns recorded without one, read as an
-- era nobody can place — which has to mean "do not filter" rather than "stale".
ALTER TABLE turns ADD COLUMN base_sha TEXT;
