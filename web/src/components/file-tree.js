/**
 * The file list beside the diff, as a jump list.
 *
 * Every file in the range is already in the diff, so picking one out of the
 * tree is a scroll rather than a round trip. Which node is highlighted follows
 * the scroller instead of the click, so it stays honest when the reader scrolls
 * past a file on their own.
 */
export class FileTree extends HTMLElement {
  connectedCallback() {
    this.lines = document.querySelector("#diff-lines");
    if (!this.lines) return;

    this.onClick = (event) => {
      const node = event.target.closest(".file-node");
      if (!node) return;

      event.preventDefault();
      // Not the browser's own hash navigation: that pushes history, which htmx
      // then has to reconcile against a fragment it never navigated to.
      document.querySelector(node.getAttribute("href"))?.scrollIntoView({ block: "start" });
    };
    this.addEventListener("click", this.onClick);

    // The file being read is the first one not yet scrolled past, which is what
    // the sticky header is showing. The observer is only the trigger — the
    // answer is measured, because a section can cross the whole scrollport
    // between two callbacks without reporting intersecting at either end, which
    // leaves anything derived from the entries stale and wrong.
    //
    // NB: the root is pulled in a pixel at the top on purpose. A file scrolled
    // exactly to the top leaves its predecessor's bottom edge flush with the
    // scroller's, and a flush edge still counts as intersecting — so without
    // this the tree names the file just left rather than the one arrived at.
    // It is the same fudge the geometry this replaced carried as `top + 1`.
    this.observer = new IntersectionObserver(() => this.mark(), {
      root: this.lines,
      threshold: [0, 1],
    });

    this.watch();
    // A morph brings new files in without this element being rebuilt, so the
    // set to observe is re-read once each update has settled.
    this.onSettle = () => this.watch();
    document.addEventListener("htmx:after:settle", this.onSettle);
  }

  watch() {
    this.observer?.disconnect();

    this.sections = [...(this.lines?.querySelectorAll(".file") ?? [])];
    // Looked up once here rather than per callback, and rebuilt on each settle
    // because a morph may have replaced the nodes these point at.
    this.nodes = new Map(
      this.sections.map((section) => [section, this.querySelector(`[href="#${section.id}"]`)]),
    );
    // NB: cleared, not kept. `selected` is ours — the server never renders it,
    // so every morph strips it off. Forgetting which file was being read is
    // what makes the next callback put the highlight back rather than decide
    // nothing has changed.
    this.reading = null;

    for (const section of this.sections) this.observer?.observe(section);
  }

  mark() {
    if (!this.sections?.length) return;

    // NB: reads only, in one pass, so this is one layout flush however many
    // files it walks — not one per file.
    const top = this.lines.getBoundingClientRect().top;
    const reading =
      this.sections.find((section) => section.getBoundingClientRect().bottom > top + 1) ??
      this.sections.at(-1);

    // Everything below is a write, and only when the answer moved — which is
    // what keeps that flush from being followed by another.
    if (reading === this.reading) return;

    this.nodes.get(this.reading)?.classList.remove("selected");
    this.nodes.get(reading)?.classList.add("selected");
    this.reading = reading;
    this.nodes.get(reading)?.scrollIntoView({ block: "nearest" });
  }

  disconnectedCallback() {
    this.observer?.disconnect();
    this.removeEventListener("click", this.onClick);
    document.removeEventListener("htmx:after:settle", this.onSettle);
    this.observer = null;
  }
}
