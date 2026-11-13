<script lang="ts">
  // Click, press a combination. Backspace/Delete clears, Escape cancels. Produces the
  // accelerator syntax the global-shortcut plugin parses ("Ctrl+Alt+F10").
  let { value, onchange }: { value: string; onchange: (v: string) => void } = $props();
  let recording = $state(false);
  let hint = $state("");

  function keyName(e: KeyboardEvent): string | null {
    const c = e.code;
    if (/^Key[A-Z]$/.test(c)) return c.slice(3);
    if (/^Digit\d$/.test(c)) return c.slice(5);
    if (/^F([1-9]|1\d|2[0-4])$/.test(c)) return c;
    if (/^(Control|Alt|Shift|Meta|OS)(Left|Right)?$/.test(c)) return null;
    return c; // Numpad1, Backquote, PrintScreen, Insert...
  }

  function onKey(e: KeyboardEvent) {
    if (!recording) return;
    e.preventDefault();
    e.stopPropagation();
    if (e.key === "Escape") return stop();
    const mods = [e.ctrlKey && "Ctrl", e.altKey && "Alt", e.shiftKey && "Shift", e.metaKey && "Super"].filter(
      Boolean,
    ) as string[];
    if ((e.key === "Backspace" || e.key === "Delete") && mods.length === 0) {
      onchange("");
      return stop();
    }
    const key = keyName(e);
    if (!key) return;
    // A bare key would be taken from every app; F13-F24 are free to use alone.
    if (mods.length === 0 && !/^F(1[3-9]|2[0-4])$/.test(key)) {
      hint = "Add Ctrl, Alt or Shift";
      return;
    }
    onchange([...mods, key].join("+"));
    stop();
  }

  function stop() {
    recording = false;
    hint = "";
  }
</script>

<button
  class="hotkey"
  class:recording
  onclick={() => (recording = !recording)}
  onkeydown={onKey}
  onblur={stop}
  aria-label="Shortcut: {value || 'none'}. Activate to record a new one."
>
  {#if recording}
    <span class="rec">{hint || "Press a shortcut…"}</span>
  {:else if value}
    {#each value.split("+") as k, i (i)}<kbd>{k}</kbd>{/each}
  {:else}
    <span class="none">Not set</span>
  {/if}
</button>

<style>
  .hotkey {
    display: inline-flex;
    gap: 4px;
    align-items: center;
    min-width: 172px;
    height: 32px;
    padding: 0 8px;
    border: 1px solid var(--line-strong);
    border-radius: var(--radius-sm);
    background: var(--surface-2);
    cursor: pointer;
    transition: border-color var(--fast) var(--ease);
  }
  .hotkey:hover {
    border-color: var(--text-3);
  }
  .recording {
    border-color: var(--accent);
    background: var(--accent-soft);
  }
  kbd {
    padding: 1px 6px;
    border: 1px solid var(--line-strong);
    border-bottom-width: 2px;
    border-radius: 4px;
    background: var(--surface);
    font: 12px var(--font);
  }
  .none,
  .rec {
    color: var(--text-3);
    font-size: 13px;
  }
  .rec {
    color: var(--text);
  }
</style>
