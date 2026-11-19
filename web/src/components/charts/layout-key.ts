import { createContext } from "react";

/**
 * A key that a chart's owner changes in the render that resizes the chart's box itself: a
 * dashboard cell redrawn at a new width after a window resize, or after its own resize. A chart
 * under a key measures its box in that same render, so it never shows a frame at its old size
 * (a ResizeObserver would tell it one frame late). Empty outside such an owner: the chart measures
 * itself as before.
 */
export const LayoutKey = createContext("");
