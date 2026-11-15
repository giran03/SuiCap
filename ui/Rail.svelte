<script lang="ts">
  import { onMount } from "svelte";

  interface Props {
    scrollEl: HTMLElement | undefined;
    mode?: "replay" | "continuous";
    armed: boolean;
    recording?: boolean;
    paused?: boolean;
    hasError: boolean;
    settingsOpen: boolean;
    upBadge: boolean;
    onOpenSettings: () => void;
  }

  let {
    scrollEl,
    mode = "replay",
    armed,
    recording = false,
    paused = false,
    hasError,
    settingsOpen,
    upBadge,
    onOpenSettings,
  }: Props = $props();

  const ITEMS = [
    {
      id: "replay",
      label: "Replay",
      tip: "Replay · buffer & status",
      icon: "M3 12a9 9 0 1 0 3-6.7L3 8M3 3v5h5M12 7v5l3 2",
    },
    {
      id: "video",
      label: "Video",
      tip: "Video · source & quality",
      icon: "M3 6.5A2.5 2.5 0 0 1 5.5 4h8A2.5 2.5 0 0 1 16 6.5v11a2.5 2.5 0 0 1-2.5 2.5h-8A2.5 2.5 0 0 1 3 17.5zM16 10l5-3v10l-5-3z",
    },
    {
      id: "audio",
      label: "Audio",
      tip: "Audio · tracks & app routing",
      icon: "M11 5L6 9H3v6h3l5 4zM15.5 8.5a5 5 0 0 1 0 7M18.5 5.5a9 9 0 0 1 0 13",
    },
    {
      id: "clips",
      label: "Clips",
      tip: "Clips · library & storage",
      icon: "M4 5h16a1 1 0 0 1 1 1v12a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V6a1 1 0 0 1 1-1zM10 9.5l5 2.5-5 2.5z",
    },
  ] as const;

  let active = $state<string>("replay");
  let navEl = $state<HTMLElement>();
  let indicator = $state({ y: 0, h: 0, ready: false });
  let pinnedId: string | null = null;
  let pinnedY: number | null = null;
  let spyPausedUntil = 0;
  const PIN_RELEASE_PX = 160;

  function measure() {
    const el = navEl?.querySelector<HTMLElement>('[data-id="' + active + '"]');
    if (el) indicator = { y: el.offsetTop, h: el.offsetHeight, ready: true };
  }

  $effect(() => {
    void active;
    measure();
  });

  function spy() {
    if (!scrollEl || performance.now() < spyPausedUntil) return;
    if (pinnedId) {
      if (pinnedY === null || Math.abs(scrollEl.scrollTop - pinnedY) < PIN_RELEASE_PX) {
        if (active !== pinnedId) active = pinnedId;
        return;
      }
      pinnedId = null;
      pinnedY = null;
    }
    const containerTop = scrollEl.getBoundingClientRect().top;
    const line = containerTop + scrollEl.clientHeight * 0.3;
    let best: { id: string; top: number; width: number } | null = null;
    for (const item of ITEMS) {
      const el = document.getElementById(item.id);
      if (!el) continue;
      const rect = el.getBoundingClientRect();
      if (rect.top > line) continue;
      if (!best || rect.top > best.top + 40 || (Math.abs(rect.top - best.top) <= 40 && rect.width > best.width)) {
        best = { id: item.id, top: rect.top, width: rect.width };
      }
    }
    const atBottom = scrollEl.clientHeight + scrollEl.scrollTop >= scrollEl.scrollHeight - 4;
    const current = atBottom ? ITEMS[ITEMS.length - 1].id : (best?.id ?? ITEMS[0].id);
    if (current !== active) active = current;
  }

  function go(id: string) {
    const target = document.getElementById(id);
    if (!target) return;
    active = id;
    pinnedId = id;
    pinnedY = null;
    spyPausedUntil = performance.now() + 800;
    const reduced = matchMedia("(prefers-reduced-motion: reduce)").matches;
    target.scrollIntoView({ behavior: reduced ? "auto" : "smooth", block: "start" });
    setTimeout(() => {
      if (pinnedId === id && scrollEl) pinnedY = scrollEl.scrollTop;
    }, 900);
  }

  function scrollTop() {
    if (!scrollEl) return;
    active = ITEMS[0].id;
    pinnedId = ITEMS[0].id;
    pinnedY = 0;
    spyPausedUntil = performance.now() + 800;
    const reduced = matchMedia("(prefers-reduced-motion: reduce)").matches;
    scrollEl.scrollTo({ top: 0, behavior: reduced ? "auto" : "smooth" });
  }

  $effect(() => {
    const el = scrollEl;
    if (!el) return;
    let frame = 0;
    const onScroll = () => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(spy);
    };
    const onUserInterrupt = () => {
      if (performance.now() >= spyPausedUntil && !pinnedId) return;
      spyPausedUntil = 0;
      pinnedId = null;
      pinnedY = null;
      onScroll();
    };
    el.addEventListener("scroll", onScroll, { passive: true });
    el.addEventListener("wheel", onUserInterrupt, { passive: true });
    el.addEventListener("pointerdown", onUserInterrupt, { passive: true });
    el.addEventListener("touchstart", onUserInterrupt, { passive: true });
    spy();
    return () => {
      cancelAnimationFrame(frame);
      el.removeEventListener("scroll", onScroll);
      el.removeEventListener("wheel", onUserInterrupt);
      el.removeEventListener("pointerdown", onUserInterrupt);
      el.removeEventListener("touchstart", onUserInterrupt);
    };
  });

  onMount(() => {
    const ro = new ResizeObserver(measure);
    if (navEl) ro.observe(navEl);
    window.addEventListener("resize", measure);
    measure();
    return () => {
      ro.disconnect();
      window.removeEventListener("resize", measure);
    };
  });
</script>

<aside class="rail" aria-label="Dashboard navigation">
  <button type="button" class="brand-mark" aria-label="Scroll to top" title="Scroll to top" onclick={scrollTop}>
    <svg viewBox="0 0 256 256" aria-hidden="true">
      <path d="M48.3 36.0L76.3 52.17V203.83L48.3 220.0Z" fill="currentColor" />
      <path d="M88.3 59.09L116.3 75.26V180.74L88.3 196.91Z" fill="currentColor" />
      <path d="M128.3 82.19L156.3 98.35V157.65L128.3 173.81Z" fill="currentColor" />
      <path d="M168.3 105.28L207.7 128L168.3 150.72Z" fill="var(--accent)" />
    </svg>
  </button>

  <nav bind:this={navEl} aria-label="Sections">
    <span
      class="indicator"
      class:ready={indicator.ready}
      style:transform={"translateY(" + indicator.y + "px)"}
      style:height={indicator.h + "px"}
      aria-hidden="true"
    ></span>
    {#each ITEMS as item (item.id)}
      <button
        type="button"
        class="rail-item"
        class:active={active === item.id}
        data-id={item.id}
        aria-current={active === item.id ? "location" : undefined}
        title={item.tip}
        onclick={() => go(item.id)}
      >
        <span class="icon">
          <svg viewBox="0 0 24 24" aria-hidden="true"><path d={item.icon} /></svg>
          {#if item.id === "replay" && (armed || hasError)}
            <i class="dot" class:live={armed} class:rec={recording && !paused} class:paused={recording && paused} class:err={hasError} aria-hidden="true"></i>
          {/if}
        </span>
        <span class="label">{item.id === "replay" && mode === "continuous" ? "Record" : item.label}</span>
      </button>
    {/each}
  </nav>

  <div class="rail-drag" data-tauri-drag-region aria-hidden="true"></div>

  <div class="bottom">
    <button
      type="button"
      class="rail-item"
      class:chosen={settingsOpen}
      aria-label="Settings"
      aria-haspopup="dialog"
      aria-expanded={settingsOpen}
      title="Settings"
      onclick={onOpenSettings}
    >
      <span class="icon">
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <path
            d="M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6zM19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3h0a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5h0a1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8v0a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z"
          />
        </svg>
        {#if upBadge}<span class="pip" aria-label="Update available"></span>{/if}
      </span>
      <span class="label">Settings</span>
    </button>
  </div>
</aside>

<style>
  .rail {
    grid-row: 1;
    grid-column: 1;
    display: flex;
    flex-direction: column;
    align-items: center;
    padding: 12px 8px 16px;
    gap: 18px;
    border-right: 1px solid var(--line);
    background: var(--sidebar);
    user-select: none;
  }
  .brand-mark {
    display: grid;
    place-items: center;
    width: 42px;
    height: 42px;
    padding: 0;
    border: 0;
    border-radius: 12px;
    background: transparent;
    color: var(--text);
    cursor: pointer;
    transition:
      background var(--fast) var(--ease),
      transform var(--med) var(--spring);
  }
  .brand-mark:hover {
    background: var(--surface-2);
  }
  .brand-mark:active {
    transform: scale(0.94);
  }
  .brand-mark svg {
    width: 26px;
    height: 26px;
  }
  nav {
    position: relative;
    display: grid;
    gap: 6px;
    width: 100%;
  }
  .rail-drag {
    flex: 1;
    width: 100%;
  }
  .bottom {
    display: grid;
    gap: 6px;
    width: 100%;
  }

  .indicator {
    position: absolute;
    left: 0;
    right: 0;
    top: 0;
    border-radius: 14px;
    pointer-events: none;
    opacity: 0;
    background: var(--accent-soft);
    box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--accent) 26%, transparent);
    transition:
      transform 0.42s cubic-bezier(0.22, 1, 0.36, 1),
      height 0.42s cubic-bezier(0.22, 1, 0.36, 1),
      opacity 0.2s var(--ease);
  }
  .indicator.ready {
    opacity: 1;
  }
  .indicator::before {
    content: "";
    position: absolute;
    left: -8px;
    top: 24%;
    bottom: 24%;
    width: 3px;
    border-radius: 0 3px 3px 0;
    background: var(--accent);
    box-shadow: 0 0 10px color-mix(in srgb, var(--accent) 60%, transparent);
  }

  .rail-item {
    position: relative;
    z-index: 1;
    width: 100%;
    height: 54px;
    padding: 0;
    border: 0;
    border-radius: 14px;
    background: transparent;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 5px;
    color: var(--text-3);
    text-decoration: none;
    cursor: pointer;
    transition:
      color 0.2s var(--ease),
      background 0.2s var(--ease),
      transform var(--med) var(--spring);
  }
  .rail-item:hover {
    color: var(--text);
    background: var(--surface-2);
  }
  .rail-item:active {
    transform: scale(0.96);
  }
  .rail-item.active {
    color: var(--accent);
    background: transparent;
  }
  .rail-item.chosen {
    color: var(--text);
    background: var(--surface-3);
  }
  .rail-item:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }

  .icon {
    position: relative;
    display: grid;
    place-items: center;
    transition: transform 0.35s cubic-bezier(0.22, 1, 0.36, 1);
  }
  .icon svg {
    width: 20px;
    height: 20px;
    fill: none;
    stroke: currentColor;
    stroke-width: 1.9;
    stroke-linecap: round;
    stroke-linejoin: round;
  }
  .rail-item.active .icon {
    transform: translateY(-1px) scale(1.08);
  }
  .label {
    font-size: 9.5px;
    font-weight: 600;
    letter-spacing: 0.01em;
    line-height: 1;
  }

  .dot {
    position: absolute;
    top: -3px;
    right: -5px;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    border: 2px solid var(--sidebar);
    background: var(--text-3);
    box-sizing: content-box;
  }
  .dot.live {
    background: var(--accent);
    box-shadow: 0 0 8px color-mix(in srgb, var(--accent) 70%, transparent);
  }
  .dot.rec {
    background: #ef4444;
    box-shadow: 0 0 8px color-mix(in srgb, #ef4444 70%, transparent);
  }
  .dot.paused {
    background: #f59e0b;
    box-shadow: 0 0 8px color-mix(in srgb, #f59e0b 65%, transparent);
  }
  .dot.err {
    background: var(--warn);
  }
  .pip {
    position: absolute;
    top: -2px;
    right: -3px;
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--accent);
    box-shadow: 0 0 0 2px var(--sidebar);
  }

  @media (prefers-reduced-motion: reduce) {
    .indicator,
    .icon,
    .rail-item {
      transition: none;
    }
  }
</style>
