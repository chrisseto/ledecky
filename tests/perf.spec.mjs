import { expect, test } from "./support/fixtures.mjs";

import {
  addCard,
  addProject,
  cardIn,
  editWorktree,
  moveCard,
  openCard,
} from "./support/board.mjs";

/**
 * What a large diff costs the browser, counted rather than timed.
 *
 * Identical runs swing about twofold on wall clock, so nothing here asserts a
 * duration. Everything below is a count the engine reports exactly for a fixed
 * interaction — elements, layout objects, style recalcs, forced layouts — and
 * the assertions are ceilings on those. Durations are printed for a reader and
 * asserted on by nobody.
 *
 * Not part of the ordinary loop: it seeds a diff big enough to be slow on
 * purpose. `pnpm e2e:perf` runs it.
 */
test.describe.configure({ mode: "serial" });

const TITLE = "Chew through a large diff";

/**
 * Twenty files of five hundred lines.
 *
 * Under `MAX_LINES` apiece, so no file is held back and nothing has to be
 * clicked open — the measurement is of one render, with no expansion state to
 * get wrong. Stacked files rather than one enormous file because that is the
 * shape that actually lags: every one of them is on the page at once.
 */
const FILES = 20;
const LINES = 500;

/** How many files to scroll through while counting what each scroll costs. */
const SCROLLS = 10;

const source = (n) =>
  Array.from({ length: LINES }, (_, i) => `fn item_${n}_${i}() -> usize { ${i} }`).join("\n");

let projectUrl;
let cardId;

test.beforeAll(async ({ browser }) => {
  const page = await browser.newPage();
  projectUrl = await addProject(page);
  await addCard(page, projectUrl, { title: TITLE, base: "main" });
  cardId = await cardIn(page, "todo", TITLE).getAttribute("data-card-id");
  await moveCard(page, cardId, "in_progress");
  await page.close();
});

test("a large diff, counted @perf", async ({ page }) => {
  // Deliberately the slowest thing in the suite: twenty files through git and
  // delta, then ten thousand lines rendered and scrolled.
  test.setTimeout(120_000);

  for (let n = 0; n < FILES; n++) editWorktree(cardId, `src/file_${n}.rs`, source(n));

  // Count the forced layouts the pane does to itself. Patched before any
  // script runs, so the tally covers the pane's own code rather than ours.
  await page.addInitScript(() => {
    window.__rects = 0;
    const real = Element.prototype.getBoundingClientRect;
    Element.prototype.getBoundingClientRect = function () {
      window.__rects++;
      return real.apply(this, arguments);
    };
  });

  // Every response that carries a pane, whichever route drew it: the tick and
  // the comment post both answer with the whole thing, same as `/diff`.
  let bytes = 0;
  const counted = [];
  page.on("response", (response) => {
    if (!new URL(response.url()).pathname.startsWith(`/cards/${cardId}`)) return;
    counted.push(
      response
        .body()
        .then((b) => { bytes += b.length; })
        .catch(() => {}),
    );
  });
  const settled = async () => { await Promise.all(counted); };

  const cdp = await page.context().newCDPSession(page);
  await cdp.send("Performance.enable");

  await openCard(page, cardId);
  // The card still has a live agent, so the drawer leads with the Agent tab and
  // the review pane is `display: none` — nothing in it has a box to measure.
  await page.locator('label[for="tab-review"]').click();
  // The card's own agent has had a turn of its own, so the range holds a couple
  // of files this spec did not seed. Wait on the ones it did.
  const seeded = page.locator('#review .file[data-path^="src/file_"]');
  await expect(seeded).toHaveCount(FILES);

  const metrics = async () =>
    Object.fromEntries((await cdp.send("Performance.getMetrics")).metrics.map((m) => [m.name, m.value]));

  await settled();
  const bytesAtRender = bytes;
  const rendered = await metrics();
  const counts = await page.evaluate(() => ({
    elements: document.querySelectorAll("#review *").length,
    lines: document.querySelectorAll("#review .line").length,
    rects: window.__rects,
  }));

  // Scrolling is where the tree's observer does its work, so measure across a
  // run of scrolls rather than one.
  //
  // The wait after each one is the tree catching up — the whole job of the
  // observer is to name the file being read, so that is the observable thing
  // to wait on. A bare poll of the counter would pass before it had run.
  const ids = await seeded.evaluateAll((els) => els.map((el) => el.id));
  const walked = ids.slice(0, SCROLLS);

  const before = counts.rects;
  for (const id of walked) {
    await page.locator(`#${id}`).evaluate((el) => el.scrollIntoView({ block: "start" }));
    await expect(page.locator(".file-node.selected")).toHaveAttribute("href", `#${id}`);
  }
  const scrolled = await metrics();
  const afterScroll = await page.evaluate(() => window.__rects);

  // Ticking a file off is the browser's own disclosure, so it should cost
  // nothing at all — no request, no redraw. This number used to be the whole
  // pane.
  await settled();
  const bytesBeforeTick = bytes;
  const first = page.locator("#review .file").first();
  await first.locator(".file-head .path").click();
  await expect(first.locator(".line").first()).toBeHidden();
  await settled();
  const tickBytes = bytes - bytesBeforeTick;

  // Opening a comment box and saving it. Both are one line's business now, so
  // this is the block and the two counts beside it — not the pane.
  const bytesBeforeComment = bytes;
  const line = page.locator("#review .file[open] .line").first();
  await line.click();
  await expect(page.locator("#review .compose textarea")).toBeVisible();
  await page.locator("#review .compose textarea").fill("measured");
  await page.locator("#review .batch-label").click();
  await expect(page.locator("#review .comment", { hasText: "measured" })).toBeVisible();
  await settled();
  const commentBytes = bytes - bytesBeforeComment;

  // An expansion still redraws the whole pane, because `#review` is the swap
  // target — every line of every other file over the wire for a change that
  // touched one hunk. This is the cost the disclosure sidesteps.
  const opened = page.locator("#review .file[open]").first();
  const grown = opened.locator(".line");
  const was = await grown.count();
  await settled();
  const beforeUpdate = await metrics();
  const bytesBefore = bytes;
  await opened.getByRole("link", { name: /Expand \d+ lines above/ }).first().click();
  await expect.poll(() => grown.count()).toBeGreaterThan(was);
  await settled();
  const afterUpdate = await metrics();

  const report = {
    files: FILES,
    lines: counts.lines,
    elements: counts.elements,
    elementsPerLine: +(counts.elements / counts.lines).toFixed(2),
    renderBytes: bytesAtRender,
    nodes: rendered.Nodes,
    layoutObjects: rendered.LayoutObjects,
    recalcStyleCount: rendered.RecalcStyleCount,
    layoutCount: rendered.LayoutCount,
    rectsOnRender: counts.rects,
    rectsPerScroll: +((afterScroll - before) / walked.length).toFixed(1),
    scrollRecalcs: scrolled.RecalcStyleCount - rendered.RecalcStyleCount,
    scrollLayouts: scrolled.LayoutCount - rendered.LayoutCount,
    // A fold, which asks the server for nothing but a 204.
    tickBytes,
    // Opening a comment box and saving it.
    commentBytes,
    // One expansion, which still redraws the whole pane.
    updateBytes: bytes - bytesBefore,
    updateRecalcs: afterUpdate.RecalcStyleCount - beforeUpdate.RecalcStyleCount,
    updateLayouts: afterUpdate.LayoutCount - beforeUpdate.LayoutCount,
    updateScriptMs: +((afterUpdate.ScriptDuration - beforeUpdate.ScriptDuration) * 1000).toFixed(1),
    updateLayoutMs: +((afterUpdate.LayoutDuration - beforeUpdate.LayoutDuration) * 1000).toFixed(1),
    // Printed, never asserted on.
    layoutMs: +(scrolled.LayoutDuration * 1000).toFixed(1),
    recalcMs: +(scrolled.RecalcStyleDuration * 1000).toFixed(1),
  };
  console.log(JSON.stringify(report, null, 2));

  expect(counts.lines).toBeGreaterThanOrEqual(FILES * LINES);

  // Ceilings, not measurements: each sits well clear of where the counter
  // lands now, and well under where it sat before. The numbers this run was
  // written against, for twenty files of five hundred lines:
  //
  //                        before    after
  //   elements/line         11.05     9.05
  //   layout objects      251,003   13,143
  //   layouts per scroll        1      0.9
  //
  // Layout objects is the one that matters: off-screen files are no longer
  // laid out at all.
  //
  // NB: `rectsPerScroll` counts *calls* to `getBoundingClientRect`, not layout
  // flushes. The tree reads one rect per file to find the one being read, in a
  // single pass with no write between — so eight calls cost one flush, which is
  // what `scrollLayouts` measures and what is worth a ceiling.
  //
  // `updateBytes` is not a ceiling: an expansion still sends the whole pane
  // back, every line of every other file included. `tickBytes` is what that
  // number looks like once a change stops going through the server at all.
  expect(report.elementsPerLine).toBeLessThan(10);
  expect(report.layoutObjects).toBeLessThan(50_000);
  expect(report.scrollLayouts).toBeLessThanOrEqual(SCROLLS * 2);
  // A fold is a 204 and nothing else, and a comment is one line's block plus
  // the counts that were reading over its shoulder.
  expect(report.tickBytes).toBeLessThan(1_000);
  expect(report.commentBytes).toBeLessThan(20_000);
});
