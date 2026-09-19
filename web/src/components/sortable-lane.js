import Sortable from "sortablejs";

import { refreshBoard } from "../updates.js";

/**
 * A lane cards can be dragged into.
 *
 * SortableJS moves the DOM node; the server is told the destination lane and
 * the drop index, and owns the ordering from there. The element survives a
 * board update — same id, so morph never replaces it — which is what keeps one
 * Sortable instance alive across them.
 */
export class SortableLane extends HTMLElement {
  connectedCallback() {
    if (this.sortable) return;

    this.sortable = Sortable.create(this, {
      group: "cards",
      animation: 120,
      draggable: ".card",
      ghostClass: "card-ghost",

      onEnd: async (event) => {
        const id = event.item.dataset.cardId;
        try {
          await fetch(`/cards/${id}/move`, {
            method: "POST",
            headers: { "Content-Type": "application/x-www-form-urlencoded" },
            body: new URLSearchParams({
              lane: event.to.dataset.lane,
              index: event.newIndex,
            }),
          });
        } finally {
          // Reconciles the optimistic move above with server truth. A move that
          // succeeded announces itself anyway; this is what covers one that did
          // not, and would otherwise leave the card in the wrong lane.
          refreshBoard();
        }
      },
    });
  }

  disconnectedCallback() {
    this.sortable?.destroy();
    this.sortable = null;
  }
}
