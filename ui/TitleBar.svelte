<script lang="ts">
  // Drawn title bar, as in SuiNotch's settings window (port of the Site Builder "top app bar"):
  // a drag region with minimize and close at the right. Close hides to tray via the backend.
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { showMenu } from "./menu-state.svelte";
  const win = getCurrentWindow();
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<header
  data-tauri-drag-region
  oncontextmenu={(e) =>
    showMenu(e, [
      { label: "Minimize", run: () => win.minimize() },
      { label: "Close to tray", hint: "Alt+F4", run: () => win.close() },
    ])}
>
  <button class="min" aria-label="Minimize" onclick={() => win.minimize()}>
    <svg viewBox="0 0 10 10" aria-hidden="true"><path d="M1 5h8" /></svg>
  </button>
  <button class="close" aria-label="Close" onclick={() => win.close()}>
    <svg viewBox="0 0 10 10" aria-hidden="true"><path d="M1 1l8 8M9 1l-8 8" /></svg>
  </button>
</header>

<style>
  header {
    display: flex;
    align-items: flex-start;
    justify-content: flex-end;
    height: 44px;
    padding: 6px 8px 0 0;
    gap: 2px;
  }
  button {
    display: grid;
    place-items: center;
    width: 46px;
    height: 30px;
    padding: 0;
    border: 0;
    border-radius: 8px;
    background: transparent;
    color: var(--text-2);
    cursor: default;
    transition:
      background var(--fast) var(--ease),
      color var(--fast) var(--ease),
      transform var(--med) var(--spring);
  }
  button:hover {
    background: var(--surface-2);
    color: var(--text);
  }
  button:active {
    transform: scale(0.94);
  }
  .close:hover {
    background: #c42b1c;
    color: #fff;
  }
  svg {
    width: 10px;
    height: 10px;
    fill: none;
    stroke: currentColor;
    stroke-width: 1.1;
    stroke-linecap: round;
  }
</style>
