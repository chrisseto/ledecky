import { expect, test } from "./support/fixtures.mjs";

import { addCard, addProject, cardIn, openCard } from "./support/board.mjs";

let projectUrl;

test.beforeAll(async ({ browser }) => {
  const page = await browser.newPage();
  projectUrl = await addProject(page);
  await page.close();
});

/** Makes an untitled prompt in the open settings modal and waits for it. */
async function newPrompt(page) {
  const modal = page.locator(".modal-settings");
  const options = modal.locator("select option");
  const before = await options.count();
  await modal.getByRole("button", { name: "New" }).click();
  await expect(options).toHaveCount(before + 1);
  await expect(modal.getByLabel("Name")).toHaveValue("Untitled");
  return modal;
}

/** Makes a prompt in the open settings modal and saves it. */
async function makePrompt(page, { name, body }) {
  const modal = await newPrompt(page);
  await modal.getByLabel("Name").fill(name);
  await modal.getByLabel("Body").fill(body);
  await modal.getByRole("button", { name: "Save" }).click();
  await expect(modal.locator("select option:checked")).toHaveText(name);
  return modal;
}

test("a template renders into the task of a new card", async ({ page }) => {
  const name = `Bug ${test.info().workerIndex}-${Date.now()}`;

  await page.goto(projectUrl);
  await page.getByTitle("Settings").click();
  await makePrompt(page, { name, body: "Do: {{ task }} on {{ branch }} with {{ vcs }}" });

  await addCard(page, projectUrl, { title: "Fix it", template: name });
  const id = await cardIn(page, "todo", "Do: Fix it on main with git").getAttribute("data-card-id");

  // The card holds the rendered text, and nothing else of the template.
  await openCard(page, id);
  await expect(page.locator(".drawer-card h2")).toHaveText("Do: Fix it on main with git");
  await page.locator(".drawer-card").getByRole("link", { name: "Edit" }).click();
  await expect(page.locator(".modal-card").getByLabel("Template")).toHaveCount(0);
});

test("create more keeps the chosen template", async ({ page }) => {
  const name = `Chore ${test.info().workerIndex}-${Date.now()}`;
  await page.goto("/settings/templates");
  await makePrompt(page, { name, body: "Chore: {{ task }}" });

  await page.goto(`${projectUrl}/cards/new`);
  await page.getByLabel("Template").selectOption({ label: name });
  await page.getByLabel("Task").fill("sweep");
  await page.getByRole("button", { name: "Create more" }).click();
  await expect(page.getByLabel("Task")).toHaveValue("");
  await expect(page.getByLabel("Template").locator("option:checked")).toHaveText(name);
});

test("a template that does not render is refused with the reason", async ({ page }) => {
  const name = `Broken ${test.info().workerIndex}-${Date.now()}`;

  await page.goto("/settings/templates");
  await makePrompt(page, { name, body: "{{ task.nope.deeper }}" });

  await page.goto(`${projectUrl}/cards/new`);
  await page.getByLabel("Template").selectOption({ label: name });
  await page.getByLabel("Task").fill("Anything");
  await page.getByRole("button", { name: "Create", exact: true }).click();
  await expect(page.locator(".modal-card .error")).toContainText(`${name} did not render`);
});

test("an unknown variable is refused on save", async ({ page }) => {
  await page.goto("/settings/templates");
  const modal = await newPrompt(page);
  await modal.getByLabel("Body").fill("{% if jujutsu %}{{ tsak }}{% endif %}");
  await modal.getByRole("button", { name: "Save" }).click();
  await expect(modal.locator(".error")).toContainText("Unknown variable: tsak");
});

test("a syntax error is refused on save and keeps the text", async ({ page }) => {
  await page.goto("/settings/templates");
  const modal = await newPrompt(page);
  await modal.getByLabel("Body").fill("{% if %}");
  await modal.getByRole("button", { name: "Save" }).click();

  await expect(modal.locator(".error")).toContainText("syntax error");
  await expect(modal.getByLabel("Body")).toHaveValue("{% if %}");
});

test("actions are edited in their own tab and keep their landing flag", async ({ page }) => {
  const name = `Ship ${test.info().workerIndex}-${Date.now()}`;

  await page.goto(projectUrl);
  await page.getByTitle("Settings").click();
  const modal = page.locator(".modal-settings");
  await modal.getByRole("link", { name: "Actions" }).click();
  await expect(page).toHaveURL(/\/settings\/actions/);

  // Merge is seeded, and waits for the work to reach the base.
  await modal.getByLabel("Actions").selectOption({ label: "Merge" });
  await expect(modal.getByLabel("Body")).toHaveValue(/Land it on `\{\{ branch \}\}`/);
  await expect(modal.getByRole("checkbox")).toBeChecked();

  await makePrompt(page, { name, body: "Ship {{ task }}" });
  await expect(modal.getByRole("checkbox")).not.toBeChecked();
  await modal.getByRole("checkbox").check();
  await modal.getByRole("button", { name: "Save" }).click();
  await page.reload();
  await expect(modal.getByLabel("Actions").locator("option:checked")).toHaveText(name);
  await expect(modal.getByRole("checkbox")).toBeChecked();

  page.on("dialog", (dialog) => dialog.accept());
  await modal.getByRole("button", { name: "Delete" }).click();
  await expect(modal.getByLabel("Actions").locator("option", { hasText: name })).toHaveCount(0);
});
