import { cn } from "cn";

/**
 * The cetra mark, the Caetra c (D189, `assets/brand/`), drawn in `currentColor`: teal on light
 * backgrounds and light teal on dark ones, as the brand rules ask. At least 16 px: sized in pixels,
 * since the root font size is 13 px. Decorative: the product name sits next to it.
 */
export function CetraMark({ className }: { className?: string }) {
  return (
    <svg
      viewBox="1.8 4.5 55 55"
      aria-hidden="true"
      focusable="false"
      className={cn("size-[16px] shrink-0 text-[#0f766e] dark:text-[#2dd4bf]", className)}
    >
      <path
        d="M49.62 17.22A23 23 0 1 0 49.62 46.78"
        fill="none"
        stroke="currentColor"
        strokeWidth="7.5"
        strokeLinecap="round"
      />
      <circle cx="32" cy="32" r="8.5" fill="currentColor" />
    </svg>
  );
}
