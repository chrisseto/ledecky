import { execFileSync } from "node:child_process";
import { expect } from "@playwright/test";

import { REPO } from "./paths.mjs";

export const git = (...args) =>
  execFileSync("git", ["-C", REPO, ...args], { encoding: "utf8" }).trim();

export const refs = (pattern = "refs/kanban2/**") =>
  git("for-each-ref", "--format=%(refname)", pattern).split("\n").filter(Boolean);

export const turnRefs = (cardId) => refs(`refs/kanban2/${cardId}/turn-*`);

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

/** Fills in the new-card form and returns to the board. */
export async function addCard(page, projectUrl, { title, description, base = "main", permissions, model }) {
  await page.goto(`${projectUrl}/cards/new`);
  await page.getByLabel("Title").fill(title);
  if (description) await page.getByLabel(/Description/).fill(description);
  await page.getByLabel("Base branch").selectOption(base);
  if (permissions) await page.getByLabel("Permissions").selectOption(permissions);
  if (model) await page.getByLabel("Model").selectOption(model);
  await page.getByRole("button", { name: "Create card" }).click();
  await expect(page).toHaveURL(projectUrl);
}

export const lane = (page, key) => page.locator(`[data-lane="${key}"]`);

export const cardIn = (page, key, title) =>
  lane(page, key).locator(".card", { hasText: title });

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
