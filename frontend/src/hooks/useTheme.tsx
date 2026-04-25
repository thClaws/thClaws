import { createContext, useContext } from "react";

export type ThemeMode = "light" | "dark" | "system";
export type ResolvedTheme = "light" | "dark";

export type ThemeContextValue = {
  /** User's stored preference — may be "system". */
  mode: ThemeMode;
  /** Concrete theme currently applied ("light" | "dark"). */
  resolved: ResolvedTheme;
  /** Persist a new preference via backend IPC. */
  setMode: (m: ThemeMode) => void;
};

export const ThemeContext = createContext<ThemeContextValue | null>(null);

export function detectSystem(): ResolvedTheme {
  if (typeof window === "undefined" || !window.matchMedia) return "dark";
  return window.matchMedia("(prefers-color-scheme: light)").matches
    ? "light"
    : "dark";
}

export function useTheme(): ThemeContextValue {
  const ctx = useContext(ThemeContext);
  if (!ctx) {
    // Render-time fallback so components can be rendered in isolation
    // (tests, storybook) without a provider wrapping them.
    return {
      mode: "system",
      resolved: detectSystem(),
      setMode: () => {},
    };
  }
  return ctx;
}
