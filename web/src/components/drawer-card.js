/**
 * The drawer resizes itself; this only outlives it.
 *
 * The resizer writes an inline width that the #overlay update throws away on
 * the next lane move, so the width is mirrored onto :root — which the update
 * leaves alone — as the fraction of the viewport that board.html reads back
 * before paint.
 */
export class DrawerCard extends HTMLElement {
  connectedCallback() {
    this.observer = new MutationObserver(() => {
      const fraction = this.offsetWidth / window.innerWidth;
      document.documentElement.style.setProperty("--drawer-width", fraction);
      localStorage.setItem("drawer-width", fraction);
    });

    this.observer.observe(this, { attributeFilter: ["style"] });
  }

  disconnectedCallback() {
    this.observer?.disconnect();
    this.observer = null;
  }
}
