<script lang="ts">
  // Styled tray menu (Tauri workflow §5): solid surface, anchored by the shell, closes on
  // blur / Escape / action. The real actions live in the backend.
  import { onMount, tick } from "svelte";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { api, onStatus, type Status } from "./api";

  let status = $state<Status | null>(null);
  let menu: HTMLDivElement;
  let active = $state(-1);

  const armed = $derived(status?.phase === "armed");
  const busy = $derived(status?.phase === "starting" || status?.phase === "recovering");
  const recording = $derived(armed && Boolean(status?.recording));

  function formatDuration(secs: number): string {
    const total = Math.max(0, Math.floor(secs));
    const mins = Math.floor(total / 60);
    const rem = total % 60;
    return `${mins}:${rem.toString().padStart(2, "0")}`;
  }

  type Item = { label: string; hint?: string; run: () => Promise<unknown>; danger?: boolean; disabled?: boolean };
  const groups = $derived<Item[][]>([
    [
      { label: "Save last 15 s", run: () => api.saveClip(15), disabled: !armed },
      { label: "Save last 30 s", run: () => api.saveClip(30), disabled: !armed },
      { label: "Save last 60 s", run: () => api.saveClip(60), disabled: !armed },
      {
        label: "Save whole buffer",
        hint: status ? `${status.buffer_secs} s` : "",
        run: () => api.saveClip(null),
        disabled: !armed,
      },
      recording
        ? {
            label: "Stop recording",
            hint: formatDuration(status?.recording_secs ?? 0),
            run: () => api.recordStop(),
            danger: true,
          }
        : {
            label: "Start recording",
            run: () => api.recordStart(),
            disabled: !armed,
          },
    ],
    [
      armed || busy
        ? { label: "Turn replay off", run: () => api.disarm() }
        : { label: "Turn replay on", run: () => api.arm() },
      { label: "Open clips folder", run: () => api.openClipsFolder() },
      { label: "Settings", run: () => api.openSettings() },
    ],
    [{ label: "Quit SuiCap", run: () => api.quit(), danger: true }],
  ]);
  const flat = $derived(groups.flat());

  const headline = $derived.by(() => {
    if (!status) return "SuiCap";
    switch (status.phase) {
      case "armed":
        return status.recording
          ? `Recording · ${formatDuration(status.recording_secs)}`
          : `Replay on · ${Math.min(status.buffered_secs, status.buffer_secs).toFixed(0)} of ${status.buffer_secs} s`;
      case "starting":
        return "Starting capture…";
      case "recovering":
        return "Restarting capture…";
      case "error":
        return "Capture stopped";
      default:
        return "Replay off";
    }
  });

  async function run(item: Item) {
    if (item.disabled) return;
    try {
      await item.run();
    } finally {
      getCurrentWindow().close();
    }
  }

  function step(dir: number) {
    const n = flat.length;
    let i = active;
    for (let k = 0; k < n; k++) {
      i = (i + dir + n) % n;
      if (!flat[i].disabled) break;
    }
    active = i;
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") getCurrentWindow().close();
    else if (e.key === "ArrowDown") (step(1), e.preventDefault());
    else if (e.key === "ArrowUp") (step(-1), e.preventDefault());
    else if ((e.key === "Enter" || e.key === " ") && active >= 0) (run(flat[active]), e.preventDefault());
  }

  onMount(() => {
    const un = onStatus((s) => (status = s));
    (async () => {
      status = await api.status();
      await tick();
      // Layout size, not the bounding box: the open animation scales the menu.
      // The window is exactly the card.
      await api.menuReady(menu.offsetWidth, menu.offsetHeight);
    })();
    return () => {
      un.then((f) => f());
    };
  });
</script>

<svelte:window onkeydown={onKey} />

<div class="menu" bind:this={menu} role="menu" aria-label="SuiCap">
  <div class="head">
    <span class="dot" class:on={armed} class:err={status?.phase === "error"}></span>
    <span>{headline}</span>
  </div>
  {#each groups as group, g (g)}
    <div class="sep"></div>
    {#each group as item (item.label)}
      {@const i = flat.indexOf(item)}
      <button
        role="menuitem"
        class="item"
        style="--i: {i}"
        class:danger={item.danger}
        class:active={i === active}
        disabled={item.disabled}
        onmouseenter={() => (active = item.disabled ? -1 : i)}
        onclick={() => run(item)}
      >
        <span>{item.label}</span>
        {#if item.hint}<span class="hint">{item.hint}</span>{/if}
      </button>
    {/each}
  {/each}
</div>

<style>
  :global(html.menu-window),
  :global(html.menu-window body) {
    background: transparent;
    overflow: hidden;
  }

  .menu {
    position: absolute;
    top: 0;
    left: 0;
    width: 252px;
    padding: 6px;
    box-sizing: border-box;
    background: var(--surface-solid); /* opaque: never blurred (workflow §5.7) */
    border-radius: 14px;
    border: 1px solid var(--line-strong); /* the window is exactly the card: no shadow, no margin */
    transform-origin: 50% 100%;
    animation: open var(--med) var(--spring);
    font-size: 13px;
  }

  @keyframes open {
    from {
      opacity: 0;
      transform: scale(0.97);
    }
  }

  .head {
    display: flex;
    align-items: center;
    gap: 9px;
    padding: 8px 10px 7px;
    color: var(--text-2);
    font-size: 12px;
    font-weight: 550;
  }

  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--text-3);
    flex: none;
    transition: background var(--med) var(--ease);
  }
  .dot.on {
    background: var(--accent);
    animation: pulse 2.4s var(--ease) infinite;
  }
  .dot.err {
    background: var(--warn);
  }
  @keyframes pulse {
    0% {
      box-shadow: 0 0 0 0 var(--accent-soft);
    }
    70%,
    100% {
      box-shadow: 0 0 0 6px transparent;
    }
  }

  .sep {
    height: 1px;
    margin: 5px 8px;
    background: var(--line);
  }

  .item {
    display: flex;
    width: 100%;
    align-items: center;
    justify-content: space-between;
    padding: 7px 10px;
    border: 0;
    border-radius: var(--radius-sm);
    background: transparent;
    font-weight: 500;
    text-align: left;
    cursor: default;
    animation: item-in var(--med) var(--ease) both;
    animation-delay: calc(var(--i) * 14ms);
    transition:
      background var(--fast) var(--ease),
      transform var(--med) var(--spring);
  }
  @keyframes item-in {
    from {
      opacity: 0;
      transform: translateX(-6px);
    }
  }
  .item.active {
    background: var(--surface-3);
  }
  .item:active:not(:disabled) {
    background: var(--line-strong);
    transform: scale(0.985);
  }
  .item:disabled {
    color: var(--text-3);
  }
  .item:focus-visible {
    outline-offset: -2px;
  }
  .danger {
    color: var(--rec);
  }
  .danger.active {
    background: var(--rec-soft);
  }
  .hint {
    color: var(--text-3);
    font-size: 12px;
    font-variant-numeric: tabular-nums;
  }
</style>
