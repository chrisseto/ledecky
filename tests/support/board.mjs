import { execFileSync } from "node:child_process";
import { expect } from "@playwright/test";

import { REPO } from "./paths.mjs";

export const git = (...args) =>
  execFileSync("git", ["-C", REPO, ...args], { encoding: "utf8" }).trim();

export const refs = (pattern = "refs/ledecky/**") =>
  git("for-each-ref", "--format=%(refname)", pattern).split("\n").filter(Boolean);

export const turnRefs = (cardId) => refs(`refs/ledecky/${cardId}/turn-*`);

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

/** Registers the scratch repository as a project and returns its board URL. */
export async function addProject(page) {
  await page.goto("/projects/new");
  await page.getByLabel("Repository directory").fill(REPO);
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
export async function addCard(page, projectUrl, { title, description, base = "main", permissions, model }) {
  await page.goto(`${projectUrl}/cards/new`);
  await page.getByLabel("Task").fill(description ? `${title}\n\n${description}` : title);
  await page.getByLabel("Base branch").fill(base);
  if (permissions) await page.getByLabel("Permissions").selectOption(permissions);
  if (model) await page.getByLabel("Model").selectOption(model);
  await page.getByRole("button", { name: "Create", exact: true }).click();
  await expect(page).toHaveURL(projectUrl);
}

export const lane = (page, key) => page.locator(`[data-lane="${key}"]`);

export const cardIn = (page, key, title) =>
  lane(page, key).locator(".card", { hasText: title });

/** Opens a card's drawer over the board. */
export async function openCard(page, cardId) {
  await page.goto(`/cards/${cardId}`);
  await expect(page.locator(".drawer-card")).toBeVisible();
}

/** The stacked diff renders every changed file; this is one of them. */
export const fileSection = (page, path) => page.locator(`#review .file[data-path="${path}"]`);

/** Types a review comment on a diff line and clicks away, which saves it. */
export async function comment(page, line, body) {
  await line.click();
  await page.locator(".compose textarea").fill(body);
  // Blur is the save; the file's own header is the nearest thing that is not a
  // diff line and cannot scroll out from under the click.
  await line.locator("xpath=ancestor::section[@class='file']").locator(".file-head .path").click();
}

/**
 * Collects the status of every board poll, ignoring the navigation that loaded
 * the page. Call before `goto`; the returned array fills as ticks land.
 */
export function pollsOf(page, projectUrl) {
  const path = new URL(projectUrl).pathname;
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
