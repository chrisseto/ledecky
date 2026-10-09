import { existsSync } from "node:fs";
import { join } from "node:path";

import { expect, test } from "./support/fixtures.mjs";
import { SLOW } from "../playwright.config.mjs";

import {
  addCard,
  addProject,
  cardIn,
  fileSection,
  jj,
  jjRepoGit,
  openCard,
  runAction,
  openReview,
  moveCard,
  worktreeOf,
} from "./support/board.mjs";
import { JJ_REPO } from "./support/paths.mjs";

test.describe.configure({ mode: "serial" });

const TITLE = "Teach it jujutsu";
const TASK = "Print a banner line when the program starts.";

let jjProject;
let cardId;

test.beforeAll(async ({ browser }) => {
  const page = await browser.newPage();
  jjProject = await addProject(page, JJ_REPO);
  await page.close();
});

/// The whole of "only present the option if the VCS is detected in the repo".
test("the chooser appears only for a repository that has jujutsu", async ({ page }) => {
  await page.goto(`${jjProject}/cards/new`);
  const chooser = page.getByLabel("Version control");
  await expect(chooser).toBeVisible();
  await expect(chooser.locator("option")).toHaveText(["Git", "Jujutsu"]);

  // The plain git repository offers one, and one option is not a choice. Its
  // own project, registered here so this does not depend on another spec.
  const gitProject = await addProject(page);
  await page.goto(`${gitProject}/cards/new`);
  await expect(page.getByLabel("Version control")).toHaveCount(0);
  // The rest of the form is still there, so the absence above is the chooser's
  // and not a form that failed to render.
  await expect(page.getByLabel("Model")).toBeVisible();
});

/// `vcs` is set once and describes the workspace, so the edit form has no field
/// for it — offering the choice there would be offering something the write
/// ignores.
test("the chooser is a new-card choice, not an editable one", async ({ page }) => {
  await addCard(page, jjProject, { title: "Editable", vcs: "jj" });
  const id = await cardIn(page, "todo", "Editable").getAttribute("data-card-id");

  await page.goto(`/cards/${id}/edit`);
  await expect(page.getByLabel("Task")).toBeVisible();
  await expect(page.getByLabel("Version control")).toHaveCount(0);

  // And the card keeps what it was made with across a save.
  await page.getByLabel("Task").fill("Editable\n\nReworded.");
  await page.getByRole("button", { name: "Save" }).click();
  await expect(page).toHaveURL(`/cards/${id}`);
  expect(jj(JJ_REPO, "--ignore-working-copy", "workspace", "list")).not.toContain(
    `ledecky-${id}`,
  );
});

test("a jj card gets a jj workspace, and git is never asked for one", async ({ page }) => {
  await page.goto(jjProject);
  await addCard(page, jjProject, { title: TITLE, description: TASK, vcs: "jj" });
  cardId = await cardIn(page, "todo", TITLE).getAttribute("data-card-id");

  await moveCard(page, cardId, "in_progress");

  const workspace = worktreeOf(cardId);
  await expect.poll(() => existsSync(join(workspace, ".jj")), { timeout: SLOW }).toBe(true);

  // What makes this not a git worktree: the objects are the project's, but
  // there is no `.git` here and git never registered one.
  expect(existsSync(join(workspace, ".git"))).toBe(false);
  expect(jjRepoGit("worktree", "list")).not.toContain(`worktrees/${cardId}`);
  expect(jj(JJ_REPO, "--ignore-working-copy", "workspace", "list")).toContain(
    `ledecky-${cardId}`,
  );

  // And the card's diffs have somewhere to be measured from.
  expect(
    jjRepoGit("for-each-ref", "--format=%(refname)", `refs/ledecky/${cardId}/base`),
  ).toBe(`refs/ledecky/${cardId}/base`);

  // NB: waited for here rather than left to the next test. The review pane does
  // not redraw itself when its event lands — it offers — so a pane opened while
  // the first turn is still being staged renders empty and stays that way. The
  // card reaching In Review is the agent having finished a turn.
  await expect(cardIn(page, "in_review", TITLE)).toBeVisible({ timeout: SLOW });
});

test("the agent's work shows in the diff, and jj's own state does not", async ({ page }) => {
  await openReview(page, cardId);
  // The agent edits on its opening prompt, so the file is what to wait on.
  await expect(fileSection(page, "main.rs")).toBeVisible({ timeout: SLOW });

  // The one thing a `git add -A` over a jj workspace would get wrong: the range
  // is the agent's two files and nothing of jj's own, which without the
  // `.jj/.gitignore` written at creation would carry `.jj/repo` and
  // `.jj/working_copy/tree_state` too.
  await expect
    .poll(() =>
      page.locator("#review .file").evaluateAll((files) => files.map((f) => f.dataset.path)),
    )
    .toEqual(["README.md", "main.rs"]);
});

test("work the agent commits with jj becomes an anchor", async ({ page }) => {
  const workspace = worktreeOf(cardId);
  // NB: `jj commit` moves `@-` without touching a file on disk, so the staged
  // tree is unchanged and the diff does *not* go stale — there is nothing to
  // redraw. What moves is the list of points the range can be measured from,
  // which is read when the pane renders.
  jj(workspace, "commit", "-m", "agent: landed it");

  await openReview(page, cardId);

  // `@-` moved, so `base..@-` is the agent's own commit and the picker reaches
  // it. This is the whole of `vcs::head` being right: read as `@` it would be
  // an empty commit, and read with git it would not resolve at all.
  await expect(page.locator("#range-menu")).toContainText("agent: landed it");
});

test("merging moves the bookmark and retires the card", async ({ page }) => {
  const before = jjRepoGit("rev-parse", "main");

  await openCard(page, cardId);
  await runAction(page, "Merge");

  // The bookmark reaching `refs/heads` is the whole point: the fake agent moves
  // it from the main repository, because moved from the workspace it would stay
  // unexported and this would never change.
  await expect.poll(() => jjRepoGit("rev-parse", "main"), { timeout: SLOW }).not.toBe(before);
  expect(jjRepoGit("show", "main:main.rs")).toContain(TASK);

  await page.goto(jjProject);
  await expect(cardIn(page, "done", TITLE)).toBeVisible({ timeout: SLOW });

  // The workspace goes, and its registration with it — a stale one is what
  // makes the next `workspace add` of the same name fail.
  await expect
    .poll(() => existsSync(worktreeOf(cardId)), { timeout: SLOW })
    .toBe(false);
  expect(jj(JJ_REPO, "--ignore-working-copy", "workspace", "list")).not.toContain(
    `ledecky-${cardId}`,
  );
});
