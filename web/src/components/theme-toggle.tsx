import { Moon, Sun } from "lucide-react";
import {
  applyPalette,
  type Density,
  type Palette,
  readStoredPalette,
  type Theme,
  useTheme,
} from "@/components/theme-provider";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuLabel,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";

const OPTIONS = [
  { value: "light", label: "Light" },
  { value: "dark", label: "Dark" },
  { value: "system", label: "System" },
] as const satisfies ReadonlyArray<{ value: Theme; label: string }>;

export const PALETTE_OPTIONS = [
  { value: "github", label: "GitHub" },
  { value: "colorblind", label: "Colour-blind" },
  { value: "contrast", label: "High contrast" },
  { value: "tokyo", label: "Tokyo Night" },
  { value: "nord", label: "Nord" },
] as const satisfies ReadonlyArray<{ value: Palette; label: string }>;

export const DENSITY_OPTIONS = [
  { value: "compact", label: "Compact" },
  { value: "comfortable", label: "Comfortable" },
] as const satisfies ReadonlyArray<{ value: Density; label: string }>;

/**
 * `onOpenChange` for a menu that holds `ThemeMenuItems`. Closing without a pick restores the
 * saved palette. A pick saves synchronously before the menu closes, so storage is up to date here.
 */
export function restoreSavedPaletteOnClose(open: boolean): void {
  if (!open) {
    applyPalette(readStoredPalette());
  }
}

/** Theme, palette and density radio groups. Works in a menu or in a submenu. */
export function ThemeMenuItems() {
  const { theme, setTheme, palette, setPalette, density, setDensity } = useTheme();

  return (
    <>
      <DropdownMenuRadioGroup
        value={theme}
        onValueChange={(value) => {
          setTheme(value as Theme);
        }}
      >
        {OPTIONS.map((option) => (
          <DropdownMenuRadioItem key={option.value} value={option.value}>
            {option.label}
          </DropdownMenuRadioItem>
        ))}
      </DropdownMenuRadioGroup>
      <DropdownMenuSeparator />
      <DropdownMenuLabel className="text-caption text-muted-foreground">Palette</DropdownMenuLabel>
      <DropdownMenuRadioGroup
        value={palette}
        onValueChange={(value) => {
          setPalette(value as Palette);
        }}
      >
        {PALETTE_OPTIONS.map((option) => (
          <DropdownMenuRadioItem
            key={option.value}
            value={option.value}
            onFocus={() => {
              // Radix focuses items on hover and arrow keys: preview only, do not save.
              applyPalette(option.value);
            }}
          >
            {option.label}
          </DropdownMenuRadioItem>
        ))}
      </DropdownMenuRadioGroup>
      <DropdownMenuSeparator />
      <DropdownMenuLabel className="text-caption text-muted-foreground">Density</DropdownMenuLabel>
      <DropdownMenuRadioGroup
        value={density}
        onValueChange={(value) => {
          setDensity(value as Density);
        }}
      >
        {DENSITY_OPTIONS.map((option) => (
          <DropdownMenuRadioItem key={option.value} value={option.value}>
            {option.label}
          </DropdownMenuRadioItem>
        ))}
      </DropdownMenuRadioGroup>
    </>
  );
}

export function ThemeToggle() {
  const { resolvedTheme } = useTheme();
  const Icon = resolvedTheme === "dark" ? Moon : Sun;

  return (
    <DropdownMenu onOpenChange={restoreSavedPaletteOnClose}>
      <DropdownMenuTrigger asChild>
        <Button variant="ghost" size="icon" aria-label="Theme">
          <Icon />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-48">
        <ThemeMenuItems />
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
