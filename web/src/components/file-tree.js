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
    // the sticky header is showing. Derived from geometry rather than from the
    // entries, because any one entry only reports its own file.
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
    for (const section of this.lines?.querySelectorAll(".file") ?? []) {
      this.observer?.observe(section);
    }
  }

  mark() {
    const sections = [...(this.lines?.querySelectorAll(".file") ?? [])];
    if (!sections.length) return;

    const top = this.lines.getBoundingClientRect().top;
    const reading =
      sections.find((section) => section.getBoundingClientRect().bottom > top + 1) ??
      sections[sections.length - 1];

    const nodeFor = (section) => this.querySelector(`[href="#${section.id}"]`);
    for (const section of sections) {
      nodeFor(section)?.classList.toggle("selected", section === reading);
    }
    if (reading) nodeFor(reading)?.scrollIntoView({ block: "nearest" });
  }

  disconnectedCallback() {
    this.observer?.disconnect();
    this.removeEventListener("click", this.onClick);
    document.removeEventListener("htmx:after:settle", this.onSettle);
    this.observer = null;
  }
}
