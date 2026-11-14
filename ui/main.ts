import "./theme.css";
import { mount } from "svelte";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { api, onSettings, type Theme } from "./api";
import Settings from "./Settings.svelte";
import Badge from "./Badge.svelte";
import { mountTooltips } from "./tooltip";

// The browser's own context menu is never shown (dev or release, text fields included): the
// page draws its own (ContextMenu.svelte), which has Cut / Copy / Paste for text fields.
document.addEventListener("contextmenu", (e) => e.preventDefault(), { capture: true });

if (import.meta.env.PROD) {
  document.addEventListener("keydown", (e) => {
    const k = e.key.toLowerCase();
    if (e.key === "F5" || e.key === "F12" || (e.ctrlKey && (k === "r" || k === "p" || (e.shiftKey && k === "i")))) {
      e.preventDefault();
    }
  });
}

const media = window.matchMedia("(prefers-color-scheme: light)");
let theme: Theme = "system";

/** Apply the stored theme to this page (uses theme.css tokens). */
function applyTheme(t: Theme) {
  theme = t;
  const light = t === "light" || (t === "system" && media.matches);
  document.documentElement.dataset.theme = light ? "light" : "dark";
}
media.addEventListener("change", () => applyTheme(theme));

async function start() {
  const win = getCurrentWindow();
  if (win.label === "badge") {
    document.documentElement.classList.add("badge-window");
    mount(Badge, { target: document.getElementById("app")! });
    return; // the shell shows it once it is hidden from capture
  }
  const settings = await api.configGet();
  applyTheme(settings.theme);
  onSettings((s) => applyTheme(s.theme));
  const target = document.getElementById("app")!;
  mount(Settings, { target, props: { initial: settings } });
  mountTooltips();
  await win.setTheme(settings.theme === "system" ? null : settings.theme);
  await win.show();
  await win.setFocus();
}

start();
