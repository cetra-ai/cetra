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

export function ThemeToggle() {
  const { theme, resolvedTheme, setTheme, palette, setPalette, density, setDensity } = useTheme();
  const Icon = resolvedTheme === "dark" ? Moon : Sun;

  return (
    <DropdownMenu
      onOpenChange={(open) => {
        // Closing without a pick restores the saved palette. A pick saves
        // synchronously before the menu closes, so storage is up to date here.
        if (!open) {
          applyPalette(readStoredPalette());
        }
      }}
    >
      <DropdownMenuTrigger asChild>
        <Button variant="ghost" size="icon" aria-label="Theme">
          <Icon />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-48">
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
        <DropdownMenuLabel className="text-caption text-muted-foreground">
          Palette
        </DropdownMenuLabel>
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
        <DropdownMenuLabel className="text-caption text-muted-foreground">
          Density
        </DropdownMenuLabel>
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
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
