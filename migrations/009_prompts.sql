-- Jinja templates the user edits in the settings modal.
--
-- `kind` is `template`, rendered once into a new card's task, or `action`,
-- rendered and sent to a card's agent. `lands` applies to actions only: the
-- server waits for the work to reach the card's base and then moves the card to
-- Done.
CREATE TABLE prompts (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('template', 'action')),
    name TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    lands INTEGER NOT NULL DEFAULT 0
);

-- Git refuses to move a branch that another worktree has checked out, and the
-- base is almost always checked out in the main repository. The git text tells
-- the agent so.
--
-- A bookmark that moves in a jj workspace does not go to `refs/heads` until a
-- jj command runs at the colocated root, and the server reads `refs/heads` to
-- find that the merge is complete. The jj text moves it from the root. It asks
-- for `jj commit` because the server compares the bookmark's tree with the turn
-- snapshot, and a bookmark cannot point at `@`.
INSERT INTO prompts (kind, name, lands, body) VALUES ('action', 'Merge', 1,
'The reviewer approved this work. Land it on `{{ branch }}`:

{% if jujutsu -%}
1. `jj commit` anything still described only in the working copy, so all of it is in `@-`.
2. Rebase onto `{{ branch }}` if it has moved ahead.
3. Move the bookmark from the main repository, which is what publishes it to git — `jj -R {{ repo }} bookmark set {{ branch }} -r <commit>`. Moving it from this workspace leaves it unexported.
4. Report the final commit id of `{{ branch }}`.
{%- else -%}
1. Commit anything still outstanding in this worktree.
2. `{{ branch }}` is checked out in the main repository at `{{ repo }}`, so it cannot be moved from here. Apply your commits there instead — `git -C {{ repo }} merge --ff-only <sha>`, or rebase onto `{{ branch }}` first if it has moved ahead.
3. Report the final SHA of `{{ branch }}`.
{%- endif %}

Do not push.');
