import { useEffect, useState } from "react";

/** Below this width the project sidebar starts collapsed, and the assistant panel is a sheet. */
export const NARROW_WIDTH = 1024;

export function isNarrowWindow(): boolean {
  return window.innerWidth < NARROW_WIDTH;
}

/** Whether the window is narrower than NARROW_WIDTH, updated on resize. */
export function useNarrowViewport(): boolean {
  const [narrow, setNarrow] = useState(isNarrowWindow);
  useEffect(() => {
    const onResize = () => {
      setNarrow(isNarrowWindow());
    };
    onResize();
    window.addEventListener("resize", onResize);
    return () => {
      window.removeEventListener("resize", onResize);
    };
  }, []);
  return narrow;
}
