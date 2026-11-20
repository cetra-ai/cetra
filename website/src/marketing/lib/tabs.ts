/**
 * Wires a WAI-ARIA tab list. `panels[i]` belongs to `tabs[i]`.
 * A click selects a tab. ArrowLeft and ArrowRight wrap around, Home and End jump to the ends;
 * the keys move the selection and the focus. Selecting sets `aria-selected`, `tabindex` (0 or -1)
 * and `hidden` on the panels.
 */
export function wireTabs(tabs: HTMLElement[], panels: HTMLElement[]): void {
  const select = (index: number) => {
    tabs.forEach((tab, i) => {
      tab.setAttribute("aria-selected", String(i === index));
      tab.tabIndex = i === index ? 0 : -1;
    });
    panels.forEach((panel, i) => {
      panel.hidden = i !== index;
    });
  };

  tabs.forEach((tab, i) => {
    tab.addEventListener("click", () => select(i));
    tab.addEventListener("keydown", (event) => {
      let next: number | null = null;
      if (event.key === "ArrowRight") next = (i + 1) % tabs.length;
      else if (event.key === "ArrowLeft") next = (i - 1 + tabs.length) % tabs.length;
      else if (event.key === "Home") next = 0;
      else if (event.key === "End") next = tabs.length - 1;
      if (next === null) return;
      event.preventDefault();
      select(next);
      tabs[next].focus();
    });
  });
}
