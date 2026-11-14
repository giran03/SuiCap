// One custom context menu for the whole page. Any element opens it with `showMenu(event, items)`;
// ContextMenu.svelte (mounted once) draws it. The browser's own menu is blocked in main.ts.

export type MenuItem =
  | { separator: true }
  | { header: string }
  | {
      label: string;
      run: () => unknown;
      /** Small colored dot before the label (track colors). */
      dot?: string;
      /** A check mark: this is the current choice. */
      checked?: boolean;
      danger?: boolean;
      disabled?: boolean;
      /** Dim text on the right, e.g. a shortcut or value. */
      hint?: string;
    };

export type MenuOpts = {
  /** Program shown as a header with its icon (an exe name). */
  app?: string;
  /** The Cut / Copy / Paste menu of a text field (set by ContextMenu itself). */
  text?: boolean;
};

/** A field you type into: it always gets the edit menu, whatever contains it. */
export function isTextField(t: EventTarget | null): t is HTMLInputElement | HTMLTextAreaElement {
  if (t instanceof HTMLTextAreaElement) return true;
  return t instanceof HTMLInputElement && ["text", "search", "number", "url", "email", ""].includes(t.type);
}

export const menu = $state<{ open: boolean; x: number; y: number; items: MenuItem[]; app?: string; id: number }>({
  open: false,
  x: 0,
  y: 0,
  items: [],
  app: undefined,
  id: 0,
});

/** Open the menu at the pointer (or at an element, for keyboard use). Drops empty groups. */
export function showMenu(e: MouseEvent | KeyboardEvent, items: MenuItem[], opts: MenuOpts = {}) {
  // Let the event bubble up to the window, which opens the edit menu for text fields.
  if (!opts.text && isTextField(e.target)) return;
  e.preventDefault();
  e.stopPropagation();
  let x = 0;
  let y = 0;
  if (e instanceof MouseEvent && (e.clientX || e.clientY)) {
    x = e.clientX;
    y = e.clientY;
  } else {
    const r = (e.currentTarget as HTMLElement | null)?.getBoundingClientRect();
    x = r ? r.left + 12 : 40;
    y = r ? r.bottom - 4 : 40;
  }
  menu.items = tidy(items);
  menu.app = opts.app;
  menu.x = x;
  menu.y = y;
  menu.id++;
  menu.open = menu.items.length > 0;
}

export function closeMenu() {
  menu.open = false;
}

/** No leading, trailing or doubled separators. */
function tidy(items: MenuItem[]): MenuItem[] {
  const out: MenuItem[] = [];
  for (const it of items) {
    const sep = "separator" in it;
    if (sep && (out.length === 0 || "separator" in out[out.length - 1])) continue;
    out.push(it);
  }
  while (out.length && "separator" in out[out.length - 1]) out.pop();
  return out;
}

/** Copy text; works without the async clipboard API being granted. */
export async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.style.position = "fixed";
    ta.style.opacity = "0";
    document.body.appendChild(ta);
    ta.select();
    const ok = document.execCommand("copy");
    ta.remove();
    return ok;
  }
}
