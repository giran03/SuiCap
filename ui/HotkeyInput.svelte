<script lang="ts">
  // Click, press a combination. Backspace/Delete clears, Escape cancels. Produces the
  // accelerator syntax the global-shortcut plugin parses ("Ctrl+Alt+F10").
  let { value, onchange }: { value: string; onchange: (v: string) => void } = $props();
  let recording = $state(false);
  let btn = $state<HTMLButtonElement>();

  /** Start listening for a new combination (the context menu's "Change shortcut"). */
  export function start() {
    recording = true;
    btn?.focus();
  }
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
  bind:this={btn}
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
    gap: 5px;
    align-items: center;
    min-width: 172px;
    height: 34px;
    padding: 0 9px;
    border: 0;
    border-radius: var(--radius-sm);
    background: var(--surface-2);
    box-shadow: inset 0 0 0 1px transparent;
    cursor: pointer;
    transition:
      background var(--fast) var(--ease),
      box-shadow var(--med) var(--ease),
      transform var(--med) var(--spring);
  }
  .hotkey:hover {
    background: var(--surface-3);
  }
  .hotkey:active {
    transform: scale(0.98);
  }
  .recording,
  .recording:hover {
    background: var(--accent-soft);
    box-shadow: inset 0 0 0 1.5px var(--accent);
  }
  kbd {
    padding: 1px 7px;
    border-radius: 6px;
    background: var(--surface-3);
    box-shadow: inset 0 -2px 0 var(--line);
    font: 600 12px var(--font);
    animation: key-in var(--med) var(--spring);
  }
  @keyframes key-in {
    from {
      opacity: 0;
      transform: translateY(3px);
    }
  }
  .none,
  .rec {
    color: var(--text-3);
    font-size: 12.5px;
    font-weight: 500;
  }
  .rec {
    color: var(--text);
    animation: blink 1.4s ease-in-out infinite;
  }
  @keyframes blink {
    50% {
      opacity: 0.55;
    }
  }
</style>
