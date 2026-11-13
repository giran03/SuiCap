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
        return `Replay on · ${Math.min(status.buffered_secs, status.buffer_secs).toFixed(0)} of ${status.buffer_secs} s`;
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
      const r = menu.getBoundingClientRect();
      // Window = menu + the margin its shadow draws into.
      await api.menuReady(Math.ceil(r.width + 16), Math.ceil(r.height + 16));
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
    top: 8px;
    left: 8px;
    width: 244px;
    padding: 4px;
    background: var(--surface); /* opaque: never blurred (workflow §5.7) */
    border: 1px solid var(--line-strong);
    border-radius: var(--radius);
    box-shadow: var(--shadow-menu);
    animation: open var(--fast) var(--ease);
    font-size: 13px;
  }

  @keyframes open {
    from {
      opacity: 0;
      transform: translateY(4px);
    }
  }

  .head {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 7px 10px 6px;
    color: var(--text-2);
    font-size: 12px;
  }

  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--text-3);
    flex: none;
  }
  .dot.on {
    background: var(--accent);
  }
  .dot.err {
    background: var(--warn);
  }

  .sep {
    height: 1px;
    margin: 4px 6px;
    background: var(--line);
  }

  .item {
    display: flex;
    width: 100%;
    align-items: center;
    justify-content: space-between;
    padding: 6px 10px;
    border: 0;
    border-radius: var(--radius-sm);
    background: transparent;
    text-align: left;
    cursor: default;
  }
  .item.active {
    background: var(--surface-3);
  }
  .item:active:not(:disabled) {
    background: var(--line-strong);
  }
  .item:disabled {
    color: var(--text-3);
  }
  .item:focus-visible {
    outline-offset: -2px;
  }
  .danger {
    color: var(--accent);
  }
  .danger.active {
    background: var(--accent-soft);
  }
  .hint {
    color: var(--text-3);
    font-size: 12px;
  }
</style>
