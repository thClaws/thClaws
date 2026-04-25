import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "./index.css";
import App from "./App";
import { ThemeProvider } from "./components/ThemeProvider";

// Apply the OS-preferred theme synchronously before React mounts, so the
// first paint of the startup modal already honours the user's light/dark
// preference. The ThemeProvider will overwrite this once the backend
// reports the stored mode (which takes one IPC round-trip on boot).
(() => {
  const prefersLight =
    typeof window !== "undefined" &&
    window.matchMedia &&
    window.matchMedia("(prefers-color-scheme: light)").matches;
  document.documentElement.setAttribute(
    "data-theme",
    prefersLight ? "light" : "dark",
  );
})();

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <ThemeProvider>
      <App />
    </ThemeProvider>
  </StrictMode>
);
