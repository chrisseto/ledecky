#!/usr/bin/env bash
#
# Moves an existing board across the kanban2 -> ledecky rename.
#
# The slug names more than a directory: it is baked into absolute worktree
# paths and literal ref names stored in the database, and into refs living in
# every project repository the board has ever touched. A plain `mv` leaves the
# app pointing at cards it can no longer find.
#
# Run with the server stopped. DRY_RUN=1 prints what would happen instead.
# Assumes the default data directory; if `data_dir` is set in Rocket.toml,
# point OLD_DIR and NEW_DIR at it.

set -euo pipefail

OLD=${OLD_SLUG:-kanban2}
NEW=${NEW_SLUG:-ledecky}
BASE=${XDG_DATA_HOME:-$HOME/.local/share}
OLD_DIR=${OLD_DIR:-$BASE/$OLD}
NEW_DIR=${NEW_DIR:-$BASE/$NEW}
DB="$NEW_DIR/$NEW.db"

die() { echo "error: $*" >&2; exit 1; }
note() { echo "  $*"; }

run() {
    if [[ -n ${DRY_RUN:-} ]]; then
        printf '  would run:'; printf ' %q' "$@"; printf '\n'
    else
        "$@"
    fi
}

sql() {
    if [[ -n ${DRY_RUN:-} ]]; then
        echo "  would execute: $1"
    else
        sqlite3 "$DB" "$1"
    fi
}

# Queries have to run for real even in a dry run, or there is nothing to report
# on — and in a dry run the database is still under its old name.
query() { sqlite3 "$DB_FOR_READS" "$1"; }

command -v sqlite3 >/dev/null || die "sqlite3 is not on PATH"
command -v git >/dev/null || die "git is not on PATH"

[[ -d $OLD_DIR ]] || die "$OLD_DIR does not exist — nothing to migrate"
[[ ! -e $NEW_DIR ]] || die "$NEW_DIR already exists — refusing to merge into it"
[[ -f $OLD_DIR/$OLD.db ]] || die "$OLD_DIR/$OLD.db is missing — is $OLD_DIR really a board?"

# The app keeps worktrees of its own checkout under the data directory, so it is
# easy to launch this from a directory that is about to move out from under it.
case "$PWD/" in
    "$OLD_DIR"/*) die "run this from outside $OLD_DIR — \$PWD is inside the directory being moved" ;;
esac

# A live server holds the database open and would write stale paths back over
# the migration.
if command -v fuser >/dev/null 2>&1; then
    if fuser "$OLD_DIR/$OLD.db" >/dev/null 2>&1; then
        die "$OLD_DIR/$OLD.db is open in another process — stop the server first"
    fi
else
    note "fuser is not installed — confirm the server is stopped yourself"
fi

echo "migrating $OLD_DIR -> $NEW_DIR"

# --- 1. back up the database -------------------------------------------------
# Moving the refs is the one step that is tedious to undo by hand.
run cp "$OLD_DIR/$OLD.db" "$OLD_DIR/$OLD.db.bak"

# --- 2. move the directory and the database ----------------------------------
run mv "$OLD_DIR" "$NEW_DIR"
run mv "$NEW_DIR/$OLD.db" "$DB"

if [[ -n ${DRY_RUN:-} ]]; then
    DB_FOR_READS="$OLD_DIR/$OLD.db"
else
    DB_FOR_READS="$DB"
fi

# --- 3. rewrite the paths and ref names the database stores ------------------
sql "BEGIN;
UPDATE cards SET worktree_path = replace(worktree_path, '$OLD_DIR', '$NEW_DIR')
  WHERE worktree_path IS NOT NULL;
UPDATE turns SET ref_name = replace(ref_name, 'refs/$OLD/', 'refs/$NEW/');
COMMIT;"

# --- 4. move refs/<old>/** to refs/<new>/** in every project ------------------
while IFS= read -r repo; do
    [[ -n $repo ]] || continue
    if [[ ! -d $repo ]]; then
        note "skipping $repo — no longer on disk"
        continue
    fi

    echo "refs in $repo"
    while read -r ref sha; do
        [[ -n $ref ]] || continue
        # Create before deleting, so an interruption leaves both namespaces
        # rather than neither.
        run git -C "$repo" update-ref "refs/$NEW/${ref#refs/$OLD/}" "$sha"
        run git -C "$repo" update-ref -d "$ref"
    done < <(git -C "$repo" for-each-ref --format='%(refname) %(objectname)' "refs/$OLD/**")
done < <(query "SELECT path FROM projects;")

# --- 5. point each project's worktree administrivia at the new paths ---------
# `git worktree repair` runs from the owning repository, so ask the database
# which repository owns which worktree rather than guessing from the layout.
while IFS='|' read -r repo worktree; do
    [[ -n $repo && -n $worktree ]] || continue
    if [[ ! -d $repo ]]; then
        note "skipping worktree $worktree — its project $repo is gone"
        continue
    fi
    run git -C "$repo" worktree repair "$worktree"
done < <(query "SELECT p.path, c.worktree_path FROM cards c
                JOIN projects p ON p.id = c.project_id
                WHERE c.worktree_path IS NOT NULL;")

echo "done. backup left at $NEW_DIR/$OLD.db.bak"
