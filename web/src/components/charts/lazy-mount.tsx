import { type ReactNode, useEffect, useRef, useState } from "react";

// Renders children once the placeholder nears the viewport, then keeps them mounted.
// Until then the placeholder shows `fallback`.
export function LazyMount({
  eager,
  minHeight,
  fallback,
  children,
}: {
  eager: boolean;
  minHeight: number;
  fallback: ReactNode;
  children: ReactNode;
}) {
  const [visible, setVisible] = useState(eager);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (visible) {
      return;
    }
    const node = ref.current;
    if (!node) {
      return;
    }
    const observer = new IntersectionObserver(
      (entries) => {
        if (entries.some((entry) => entry.isIntersecting)) {
          observer.disconnect();
          setVisible(true);
        }
      },
      { rootMargin: "400px 0px" },
    );
    observer.observe(node);
    return () => observer.disconnect();
  }, [visible]);

  if (visible) {
    return <>{children}</>;
  }
  return (
    <div ref={ref} style={{ minHeight }}>
      {fallback}
    </div>
  );
}
