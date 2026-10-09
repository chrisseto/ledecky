import { execFileSync } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { expect } from "@playwright/test";

import { terminalRows } from "./dom.mjs";
import { DATA_HOME, JJ_CONFIG, JJ_REPO, REPO } from "./paths.mjs";

export const git = (...args) =>
  execFileSync("git", ["-C", REPO, ...args], { encoding: "utf8" }).trim();

/** The same, against the colocated jujutsu repository. */
export const jjRepoGit = (...args) =>
  execFileSync("git", ["-C", JJ_REPO, ...args], { encoding: "utf8" }).trim();

/** `jj`, in whichever workspace or repository `cwd` names. */
export const jj = (cwd, ...args) =>
  execFileSync("jj", args, {
    cwd,
    encoding: "utf8",
    // The run's own config, for the reason `provision` writes it.
    env: { ...process.env, JJ_CONFIG },
  }).trim();

/** Where the server puts a card's worktree, as `Settings` derives it. */
export const worktreeOf = (cardId) => join(DATA_HOME, "ledecky", "worktrees", String(cardId));

/**
 * Stands in for the agent touching its own worktree.
 *
 * The fake agent only ever appends on a prompt, so work that arrives *between*
 * turns — which is most of what a real agent does — has to be made here.
 */
export const editWorktree = (cardId, path, body) => {
  const file = join(worktreeOf(cardId), path);
  // A directory the agent makes on its way is a case of its own for the
  // watcher, so writing into one has to be sayable here.
  mkdirSync(dirname(file), { recursive: true });
  writeFileSync(file, body);
};

/** The other half of `editWorktree`: work the agent took away. */
export const removeInWorktree = (cardId, path) =>
  rmSync(join(worktreeOf(cardId), path), { recursive: true, force: true });

export const worktreeGit = (cardId, ...args) =>
  execFileSync("git", ["-C", worktreeOf(cardId), ...args], { encoding: "utf8" }).trim();

/** The card's scratch directory, where its git index files live. */
export const cardDirOf = (cardId) => join(DATA_HOME, "ledecky", "cards", String(cardId));

export const refs = (pattern = "refs/ledecky/**") =>
  git("for-each-ref", "--format=%(refname)", pattern).split("\n").filter(Boolean);

export const turnRefs = (cardId) => refs(`refs/ledecky/${cardId}/turn-*`);

export const cardRefs = (cardId) => refs(`refs/ledecky/${cardId}/**`);

/** The commit a card's diffs are measured from, which follows a rebase. */
export const baseRef = (cardId) => git("rev-parse", `refs/ledecky/${cardId}/base`);

/** Lands a commit on the scratch repo itself, standing in for upstream work. */
export const commitInRepo = (path, body, subject) => {
  writeFileSync(join(REPO, path), body);
  git("add", "-A");
  git("commit", "-qm", subject);
  return git("rev-parse", "HEAD");
};

/**
 * The lines a range *adds*, without the surrounding context.
 *
 * Asserting on a whole `git diff` is misleading: context lines carry earlier
 * turns' content into a later turn's range.
 */
export const addedLines = (from, to) =>
  git("diff", from, to)
    .split("\n")
    .filter((l) => l.startsWith("+") && !l.startsWith("+++"))
    .join("\n");

/** Registers a scratch repository as a project and returns its board URL. */
export async function addProject(page, repo = REPO) {
  await page.goto("/projects/new");
  await page.getByLabel("Repository directory").fill(repo);
  await page.getByRole("button", { name: "Add project" }).click();
  await expect(page).toHaveURL(/\/projects\/\d+$/);
  return page.url();
}

/**
 * Fills in the new-card modal and returns to the board.
 *
 * The form takes one task; its first line becomes the card's title, so the
 * helper keeps the old title/description shape and joins them the way a person
 * typing into the box would.
 */
export async function addCard(
  page,
  projectUrl,
  { title, description, base = "main", permissions, model, vcs, template },
) {
  await page.goto(`${projectUrl}/cards/new`);
  if (template) await page.getByLabel("Template").selectOption({ label: template });
  await page.getByLabel("Task").fill(description ? `${title}\n\n${description}` : title);
  await pickBranch(page, base);
  if (permissions) await page.getByLabel("Permissions").selectOption(permissions);
  if (model) await page.getByLabel("Model").selectOption(model);
  // Only rendered where the project offers more than one, so only named when a
  // test means to pick.
  if (vcs) await page.getByLabel("Version control").selectOption(vcs);
  await page.getByRole("button", { name: "Create", exact: true }).click();
  await expect(page).toHaveURL(projectUrl);
}

/**
 * Takes a base out of the picker, wherever it is drawn.
 *
 * The chip is the same control in the card form and in a card's drawer, so this
 * is one helper: open it, take the row, and wait for the chip to say so. In the
 * form that is the server handing the picker back with the choice in it; on a
 * card it is the whole page coming back.
 */
export async function pickBranch(page, base) {
  const picker = page.locator(".branch-menu");
  await picker.locator("summary").click();
  await picker.getByRole("button", { name: base, exact: true }).click();
  await expect(picker.locator("summary")).toContainText(base);
}

export const lane = (page, key) => page.locator(`[data-lane="${key}"]`);

export const cardIn = (page, key, title) =>
  lane(page, key).locator(".card", { hasText: title });

/** Opens a card's drawer over the board. */
export async function openCard(page, cardId) {
  await page.goto(`/cards/${cardId}`);
  await expect(page.locator(".drawer-card")).toBeVisible();
}

/** Sends the named action from the open drawer. */
export async function runAction(page, name) {
  const drawer = page.locator(".drawer-card");
  await drawer.getByLabel("Action").selectOption({ label: name });
  await drawer.getByRole("button", { name: "Run" }).click();
}

/** The id of the named action, read from the open drawer. */
export async function actionId(page, name) {
  return page
    .locator(".drawer-card")
    .getByLabel("Action")
    .locator("option", { hasText: name })
    .getAttribute("value");
}

/**
 * Switches the open drawer to a tab.
 *
 * The tabs are a radio and its label, and the radio is what CSS keys off — so
 * the label is what a reader clicks. Named once here because which tab the
 * drawer lands on follows the agent's state, so nearly every spec that reads a
 * pane has to ask for one.
 */
export async function showTab(page, name) {
  await page.locator(`label[for="tab-${name}"]`).click();
  await expect(page.locator(`#tab-${name}`)).toBeChecked();
}

/**
 * Opens the card on its Agent tab.
 *
 * Which tab the drawer lands on follows the agent's state, and the terminal
 * does not exist until its pane does — so anything reading the screen has to
 * ask for it rather than assume the agent is still what leads.
 */
export async function openAgent(page, cardId) {
  await openCard(page, cardId);
  await showTab(page, "agent");
  await expect(terminalRows(page)).toBeVisible();
}

/**
 * Opens the card on its Review tab.
 *
 * Which tab the drawer lands on follows whether there is anything to review, so
 * anything reading the diff has to ask for it rather than assume.
 */
export async function openReview(page, cardId) {
  await openCard(page, cardId);
  await showTab(page, "review");
}

/**
 * The light that says the worktree has moved under the open pane.
 *
 * The diff does not redraw itself on a write any more — a redraw would take an
 * open comment box, the range menu and the scroll position with it — so this is
 * what the stream lights instead.
 */
export const staleDiff = (page) => page.locator("#pane-stale");

/** Waits for the stream to report the move, then takes the redraw it offers. */
export async function refreshDiff(page) {
  const stale = staleDiff(page);
  await expect(stale).toBeVisible();
  await stale.click();
  await expect(stale).toBeHidden();
}

/** The stacked diff renders every changed file; this is one of them. */
export const fileSection = (page, path) => page.locator(`#review .file[data-path="${path}"]`);

/**
 * The block of one diff line: its comments, and its box.
 *
 * Scoped to the line, not to the pane, because each line that has a draft also
 * has a box. `.compose` alone can therefore match more than one element. The
 * block is always the next element after the line, and it matches only if that
 * element is a block.
 */
export const anchoredTo = (line) => line.locator(":scope + .anchored");

/** The box that holds the draft of a line until the user sends the batch. */
export const draftBox = (line) => anchoredTo(line).locator(".compose textarea");

/**
 * Waits until no box has a save in progress. htmx's own class reports this.
 *
 * The box shows no message at other times, by design: a box that contains text
 * is already saved, so there is no "saved" message to test. A box that the user
 * types in always has a request in progress, because the throttle sends the
 * first keystroke. This wait therefore shows that the server has the text.
 */
export const saveSettled = (page) =>
  expect(page.locator("#review .compose.htmx-request")).toHaveCount(0);

/**
 * Types a review comment on a diff line and waits for the server to receive it.
 *
 * The box saves its text while the user types, so the click away only sends the
 * last part of it. Each caller then tests the draft, and `modals.spec.mjs`
 * sends the batch immediately after. A save in progress would miss that batch.
 */
export async function comment(page, line, body) {
  await line.click();
  await draftBox(line).fill(body);
  // The blur sends the text. Do not click the header of the section: the header
  // of a file is the `<summary>` of a disclosure, so a click closes the file and
  // marks it as viewed. The batch footer is the nearest element that is not a
  // line and not a control, and it cannot move before the click.
  await page.locator(".batch-label").click();
  await saveSettled(page);
}

/**
 * Collects the status of every board poll, ignoring the navigation that loaded
 * the page. Call before `goto`; the returned array fills as ticks land.
 */
export function pollsOf(page, projectUrl) {
  return pollsOfPath(page, new URL(projectUrl).pathname);
}

/** The same, for any polled fragment — the review pane polls its own URL. */
export function pollsOfPath(page, path) {
  const statuses = [];

  page.on("response", (response) => {
    if (response.request().isNavigationRequest()) return;
    if (new URL(response.url()).pathname === path) statuses.push(response.status());
  });

  return statuses;
}

/**
 * Drives a lane change the way the board's drag handler does.
 *
 * Synthesising HTML5 drag events against SortableJS is famously unreliable, and
 * what matters here is the server contract, not SortableJS itself — that is
 * covered separately by a real drag in board.spec.mjs.
 */
export async function moveCard(page, cardId, laneKey, index = 0) {
  const response = await page.request.post(`/cards/${cardId}/move`, {
    form: { lane: laneKey, index },
  });
  expect(response.status()).toBe(204);
}
