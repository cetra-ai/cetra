import { createContext, type ReactNode, useContext, useEffect, useMemo, useState } from "react";
import { STORAGE_PREFIX } from "@/product";

export type Theme = "light" | "dark" | "system";
export type ResolvedTheme = "light" | "dark";

export const THEME_STORAGE_KEY = `${STORAGE_PREFIX}theme`;
export const PALETTE_STORAGE_KEY = `${STORAGE_PREFIX}palette`;

/** Colour palettes (D-C-9). `github` is the default and sets no attribute. */
export const PALETTES = ["github", "colorblind", "contrast", "tokyo", "nord"] as const;
export type Palette = (typeof PALETTES)[number];

export function readStoredPalette(): Palette {
  try {
    const value = localStorage.getItem(PALETTE_STORAGE_KEY);
    return (PALETTES as readonly string[]).includes(value ?? "") ? (value as Palette) : "github";
  } catch {
    return "github";
  }
}

export function applyPalette(palette: Palette): void {
  const root = document.documentElement;
  if (palette === "github") {
    delete root.dataset.palette;
  } else {
    root.dataset.palette = palette;
  }
}

function writeStoredPalette(palette: Palette): void {
  try {
    localStorage.setItem(PALETTE_STORAGE_KEY, palette);
  } catch {
    // Private mode can throw. The choice still applies for this page.
  }
}

const COLOR_SCHEME_QUERY = "(prefers-color-scheme: dark)";

export function readStoredTheme(): Theme {
  try {
    const value = localStorage.getItem(THEME_STORAGE_KEY);
    if (value === "light" || value === "dark" || value === "system") {
      return value;
    }
    return "dark";
  } catch {
    return "dark";
  }
}

export function systemPrefersDark(): boolean {
  return window.matchMedia(COLOR_SCHEME_QUERY).matches;
}

export function resolveTheme(theme: Theme, prefersDark = systemPrefersDark()): ResolvedTheme {
  if (theme === "system") {
    return prefersDark ? "dark" : "light";
  }
  return theme;
}

export function applyResolvedTheme(resolved: ResolvedTheme): void {
  const root = document.documentElement;
  root.classList.remove("light", "dark");
  root.classList.add(resolved);
  root.style.colorScheme = resolved;
}

function writeStoredTheme(theme: Theme): void {
  try {
    localStorage.setItem(THEME_STORAGE_KEY, theme);
  } catch {
    // Private mode can throw. The choice still applies for this page.
  }
}

interface ThemeContextValue {
  theme: Theme;
  resolvedTheme: ResolvedTheme;
  setTheme: (theme: Theme) => void;
  palette: Palette;
  setPalette: (palette: Palette) => void;
}

const ThemeContext = createContext<ThemeContextValue | null>(null);

export function ThemeProvider({ children }: { children: ReactNode }) {
  const [theme, setThemeState] = useState<Theme>(readStoredTheme);
  const [palette, setPaletteState] = useState<Palette>(readStoredPalette);
  const [prefersDark, setPrefersDark] = useState(systemPrefersDark);
  const resolvedTheme = resolveTheme(theme, prefersDark);

  useEffect(() => {
    applyResolvedTheme(resolvedTheme);
  }, [resolvedTheme]);

  useEffect(() => {
    applyPalette(palette);
  }, [palette]);

  useEffect(() => {
    if (theme !== "system") {
      return;
    }
    const media = window.matchMedia(COLOR_SCHEME_QUERY);
    setPrefersDark(media.matches);
    const onChange = (event: MediaQueryListEvent) => {
      setPrefersDark(event.matches);
    };
    media.addEventListener("change", onChange);
    return () => {
      media.removeEventListener("change", onChange);
    };
  }, [theme]);

  const value = useMemo<ThemeContextValue>(
    () => ({
      theme,
      resolvedTheme,
      setTheme: (next) => {
        writeStoredTheme(next);
        setThemeState(next);
      },
      palette,
      setPalette: (next) => {
        writeStoredPalette(next);
        setPaletteState(next);
      },
    }),
    [palette, resolvedTheme, theme],
  );

  return <ThemeContext.Provider value={value}>{children}</ThemeContext.Provider>;
}

export function useTheme(): ThemeContextValue {
  const value = useContext(ThemeContext);
  if (!value) {
    throw new Error("useTheme must be used within ThemeProvider");
  }
  return value;
}
