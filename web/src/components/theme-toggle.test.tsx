import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";
import {
  DENSITY_STORAGE_KEY,
  PALETTE_STORAGE_KEY,
  ThemeProvider,
} from "@/components/theme-provider";
import { ThemeToggle } from "@/components/theme-toggle";

describe("ThemeToggle", () => {
  it("selects Dark from the menu", async () => {
    const user = userEvent.setup();
    render(
      <ThemeProvider>
        <ThemeToggle />
      </ThemeProvider>,
    );

    await user.click(screen.getByRole("button", { name: "Theme" }));
    await user.click(screen.getByRole("menuitemradio", { name: "Dark" }));

    expect(document.documentElement).toHaveClass("dark");
    await user.click(screen.getByRole("button", { name: "Theme" }));
    expect(screen.getByRole("menuitemradio", { name: "Dark" })).toHaveAttribute(
      "aria-checked",
      "true",
    );
  });
});

describe("ThemeToggle palettes", () => {
  beforeEach(() => {
    // The shared setup clears localStorage but not the palette attribute.
    delete document.documentElement.dataset.palette;
  });

  it("picks a palette, marks it on the page, and remembers it", async () => {
    const user = userEvent.setup();
    render(
      <ThemeProvider>
        <ThemeToggle />
      </ThemeProvider>,
    );

    await user.click(screen.getByRole("button", { name: "Theme" }));
    await user.click(screen.getByRole("menuitemradio", { name: "Nord" }));

    expect(document.documentElement.dataset.palette).toBe("nord");
    expect(localStorage.getItem(PALETTE_STORAGE_KEY)).toBe("nord");

    await user.click(screen.getByRole("button", { name: "Theme" }));
    await user.click(screen.getByRole("menuitemradio", { name: "GitHub" }));
    expect(document.documentElement.dataset.palette).toBeUndefined();
  });

  it("previews a palette on hover and restores the saved one on Escape", async () => {
    const user = userEvent.setup();
    render(
      <ThemeProvider>
        <ThemeToggle />
      </ThemeProvider>,
    );

    await user.click(screen.getByRole("button", { name: "Theme" }));
    await user.hover(screen.getByRole("menuitemradio", { name: "Tokyo Night" }));

    expect(document.documentElement.dataset.palette).toBe("tokyo");
    expect(localStorage.getItem(PALETTE_STORAGE_KEY)).toBeNull();

    await user.keyboard("{Escape}");

    expect(document.documentElement.dataset.palette).toBeUndefined();
    expect(localStorage.getItem(PALETTE_STORAGE_KEY)).toBeNull();
  });

  it("keeps a palette picked after hovering it", async () => {
    const user = userEvent.setup();
    render(
      <ThemeProvider>
        <ThemeToggle />
      </ThemeProvider>,
    );

    await user.click(screen.getByRole("button", { name: "Theme" }));
    await user.hover(screen.getByRole("menuitemradio", { name: "Nord" }));
    await user.click(screen.getByRole("menuitemradio", { name: "Nord" }));

    expect(screen.queryByRole("menu")).not.toBeInTheDocument();
    expect(document.documentElement.dataset.palette).toBe("nord");
    expect(localStorage.getItem(PALETTE_STORAGE_KEY)).toBe("nord");
  });
});

describe("ThemeToggle density", () => {
  beforeEach(() => {
    delete document.documentElement.dataset.density;
  });

  it("picks Comfortable, marks it on the page, and remembers it", async () => {
    const user = userEvent.setup();
    render(
      <ThemeProvider>
        <ThemeToggle />
      </ThemeProvider>,
    );

    await user.click(screen.getByRole("button", { name: "Theme" }));
    await user.click(screen.getByRole("menuitemradio", { name: "Comfortable" }));

    expect(document.documentElement.dataset.density).toBe("comfortable");
    expect(localStorage.getItem(DENSITY_STORAGE_KEY)).toBe("comfortable");

    await user.click(screen.getByRole("button", { name: "Theme" }));
    expect(screen.getByRole("menuitemradio", { name: "Comfortable" })).toHaveAttribute(
      "aria-checked",
      "true",
    );
    await user.click(screen.getByRole("menuitemradio", { name: "Compact" }));
    expect(document.documentElement.dataset.density).toBeUndefined();
  });
});
