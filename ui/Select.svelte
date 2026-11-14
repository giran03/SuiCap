<script lang="ts" generics="T extends string | number | null">
  // Styled dropdown (replaces the browser's <select>): the list opens as a spring-animated popup
  // beside the button, flips upward near the bottom edge, and supports arrow keys, Home / End,
  // type-ahead, Enter and Escape. Options can carry a second line and a small badge.
  import { tick } from "svelte";
  import { fade, scale } from "svelte/transition";
  import { backOut } from "svelte/easing";
  import AppIcon from "./AppIcon.svelte";

  type Option = { value: T; label: string; sub?: string; tag?: string; badge?: string; disabled?: boolean; /** Show this program's icon. */ exe?: string };

  let {
    options,
    value,
    onselect,
    label,
    disabled = false,
    minWidth = 220,
    popupMin = 0,
    fill = false,
    onopen,
  }: {
    options: Option[];
    value: T;
    onselect: (v: T) => void;
    label: string;
    disabled?: boolean;
    minWidth?: number;
    /** The list may open wider than the button, so long names fit. */
    popupMin?: number;
    /** Stretch the button to its container. */
    fill?: boolean;
    /** Called each time the list opens: refresh a list that can change (devices). */
    onopen?: () => void;
  } = $props();

  let open = $state(false);
  let btn = $state<HTMLButtonElement>();
  let list = $state<HTMLDivElement>();
  let active = $state(-1);
  let pos = $state({ x: 0, y: 0, w: 0, maxH: 320, up: false });
  let typed = "";
  let typedAt = 0;

  const current = $derived(options.find((o) => o.value === value));

  /** Move the popup to <body> so no scrolling or clipped parent can cut it off. */
  function portal(node: HTMLElement) {
    document.body.appendChild(node);
    return { destroy: () => node.remove() };
  }

  function place() {
    if (!btn) return;
    const r = btn.getBoundingClientRect();
    const rows = Math.min(options.length, 9);
    const want = rows * 40 + 12;
    const below = innerHeight - r.bottom - 12;
    const above = r.top - 12;
    const up = below < Math.min(want, 200) && above > below;
    const w = Math.min(Math.max(r.width, minWidth, popupMin), innerWidth - 16);
    pos = {
      x: Math.min(Math.max(8, r.left), innerWidth - w - 8),
      y: up ? r.top - 6 : r.bottom + 6,
      w,
      maxH: Math.max(120, Math.min(360, (up ? above : below) - 6)),
      up,
    };
  }

  async function show() {
    if (disabled || open) return;
    onopen?.();
    place();
    active = Math.max(0, options.findIndex((o) => o.value === value));
    open = true;
    await tick();
    list?.focus({ preventScroll: true });
    list?.querySelector<HTMLElement>(".opt.active")?.scrollIntoView({ block: "nearest" });
  }

  function hide(refocus = false) {
    open = false;
    if (refocus) btn?.focus();
  }

  function pick(i: number) {
    const o = options[i];
    if (!o || o.disabled) return;
    hide(true);
    if (o.value !== value) onselect(o.value);
  }

  function step(dir: number) {
    const n = options.length;
    for (let k = 1; k <= n; k++) {
      const i = (active + dir * k + n * 2) % n;
      if (!options[i].disabled) {
        active = i;
        break;
      }
    }
    tick().then(() => list?.querySelector<HTMLElement>(".opt.active")?.scrollIntoView({ block: "nearest" }));
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") (e.preventDefault(), e.stopPropagation(), hide(true));
    else if (e.key === "ArrowDown") (e.preventDefault(), step(1));
    else if (e.key === "ArrowUp") (e.preventDefault(), step(-1));
    else if (e.key === "Home") (e.preventDefault(), (active = options.findIndex((o) => !o.disabled)));
    else if (e.key === "End") (e.preventDefault(), (active = options.map((o) => !o.disabled).lastIndexOf(true)));
    else if (e.key === "Enter" || e.key === " ") (e.preventDefault(), pick(active));
    else if (e.key === "Tab") hide();
    else if (e.key.length === 1 && !e.ctrlKey && !e.metaKey) {
      // type-ahead: "mic" jumps to the first option starting with it
      const now = Date.now();
      typed = now - typedAt > 700 ? e.key : typed + e.key;
      typedAt = now;
      const i = options.findIndex((o) => !o.disabled && o.label.toLowerCase().startsWith(typed.toLowerCase()));
      if (i >= 0) {
        active = i;
        tick().then(() => list?.querySelector<HTMLElement>(".opt.active")?.scrollIntoView({ block: "nearest" }));
      }
    }
  }

  function onTriggerKey(e: KeyboardEvent) {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") (e.preventDefault(), show());
  }

  function outside(e: Event) {
    const t = e.target as Node;
    if (open && !list?.contains(t) && !btn?.contains(t)) hide();
  }
</script>

<svelte:window
  onpointerdowncapture={outside}
  onresize={() => hide()}
  onblur={() => hide()}
  onscrollcapture={(e) => open && !list?.contains(e.target as Node) && hide()}
/>

<button
  class="sel"
  class:open
  bind:this={btn}
  {disabled}
  aria-haspopup="listbox"
  aria-expanded={open}
  aria-label={label}
  style="min-width: {minWidth}px{fill ? '; width: 100%' : ''}"
  onclick={() => (open ? hide() : show())}
  onkeydown={onTriggerKey}
>
  {#if current?.exe}<AppIcon exe={current.exe} size={20} />{/if}
  {#if current?.tag}<span class="tag">{current.tag}</span>{/if}
  <span class="cur">
    <span class="cur-label">{current?.label ?? "Choose…"}</span>
    {#if current?.sub}<span class="cur-sub">{current.sub}</span>{/if}
  </span>
  <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6 9l6 6 6-6" /></svg>
</button>

{#if open}
  <div
    class="pop"
    use:portal
    bind:this={list}
    role="listbox"
    tabindex="-1"
    aria-label={label}
    style="left: {pos.x}px; width: {pos.w}px; max-height: {pos.maxH}px; {pos.up
      ? `bottom: ${innerHeight - pos.y}px; transform-origin: 50% 100%`
      : `top: ${pos.y}px; transform-origin: 50% 0`}"
    onkeydown={onKey}
    in:scale={{ duration: 240, start: 0.94, easing: backOut }}
    out:fade={{ duration: 90 }}
  >
    {#each options as o, i (i)}
      <button
        role="option"
        class="opt"
        class:active={active === i}
        class:chosen={o.value === value}
        aria-selected={o.value === value}
        disabled={o.disabled}
        tabindex="-1"
        onmouseenter={() => (active = o.disabled ? active : i)}
        onclick={() => pick(i)}
      >
        {#if o.exe}<AppIcon exe={o.exe} size={22} />{/if}
        {#if o.tag}<span class="tag">{o.tag}</span>{/if}
        <span class="txt">
          <span class="lbl">{o.label}</span>
          {#if o.sub}<span class="sub">{o.sub}</span>{/if}
        </span>
        {#if o.badge}<span class="badge">{o.badge}</span>{/if}
        {#if o.value === value}
          <svg class="tick" viewBox="0 0 24 24" aria-hidden="true"><path d="M5 12.5l4.5 4.5L19 7.5" /></svg>
        {/if}
      </button>
    {/each}
  </div>
{/if}

<style>
  .sel {
    display: inline-flex;
    align-items: center;
    justify-content: space-between;
    gap: 10px;
    max-width: 100%;
    height: 36px;
    padding: 0 10px 0 12px;
    border: 0;
    border-radius: var(--radius-sm);
    background: var(--surface-2);
    box-shadow: inset 0 0 0 1.5px transparent;
    font-size: 12.5px;
    font-weight: 550;
    text-align: left;
    cursor: pointer;
    transition:
      background var(--fast) var(--ease),
      box-shadow var(--med) var(--ease),
      transform var(--med) var(--spring);
  }
  .sel:hover:not(:disabled) {
    background: var(--surface-3);
  }
  .sel:active:not(:disabled) {
    transform: scale(0.99);
  }
  .sel.open {
    background: var(--accent-soft);
    box-shadow: inset 0 0 0 1.5px var(--accent);
  }
  .sel:disabled {
    opacity: 0.45;
    cursor: default;
  }
  .sel :global(img),
  .sel :global(.tile),
  .opt :global(img),
  .opt :global(.tile) {
    flex: none;
  }
  .cur {
    display: flex;
    flex: 1;
    min-width: 0;
    flex-direction: column;
    line-height: 1.2;
  }
  .cur-label,
  .cur-sub {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .cur-sub {
    color: var(--text-3);
    font-size: 11px;
    font-weight: 500;
  }
  .sel > svg {
    flex: none;
    width: 16px;
    height: 16px;
    fill: none;
    stroke: var(--text-3);
    stroke-width: 2.2;
    stroke-linecap: round;
    stroke-linejoin: round;
    transition: transform var(--med) var(--spring);
  }
  .sel.open > svg {
    transform: rotate(180deg);
    stroke: var(--accent);
  }

  .pop {
    position: fixed;
    z-index: 1000;
    overflow-y: auto;
    padding: 5px;
    border-radius: 13px;
    outline: 0;
    background: var(--surface-solid);
    box-shadow:
      inset 0 0 0 1px var(--line-strong),
      var(--shadow-menu);
  }
  .opt {
    display: flex;
    align-items: center;
    gap: 10px;
    width: 100%;
    min-height: 36px;
    padding: 5px 9px;
    border: 0;
    border-radius: 9px;
    background: transparent;
    color: var(--text);
    font-size: 12.5px;
    font-weight: 550;
    text-align: left;
    cursor: default;
    transition:
      background var(--fast) var(--ease),
      transform var(--med) var(--spring);
  }
  .opt.active {
    background: var(--surface-3);
  }
  .opt:active:not(:disabled) {
    transform: scale(0.99);
  }
  .opt:disabled {
    color: var(--text-3);
  }
  .opt.chosen .lbl {
    color: var(--accent);
  }
  .txt {
    display: flex;
    min-width: 0;
    flex: 1;
    flex-direction: column;
    line-height: 1.25;
  }
  .lbl,
  .sub {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .sub {
    color: var(--text-3);
    font-size: 11px;
    font-weight: 500;
  }
  .tag {
    flex: none;
    padding: 1px 6px;
    border-radius: 6px;
    background: var(--surface-3);
    color: var(--text-2);
    font-size: 10.5px;
    font-weight: 650;
    letter-spacing: 0.01em;
    font-variant-numeric: tabular-nums;
  }
  .opt.chosen .tag,
  .sel.open .tag {
    background: var(--accent-soft);
    color: var(--accent);
  }
  .badge {
    flex: none;
    padding: 1px 7px;
    border-radius: 999px;
    background: var(--surface-3);
    color: var(--text-2);
    font-size: 10.5px;
    font-weight: 650;
  }
  .tick {
    flex: none;
    width: 14px;
    height: 14px;
    fill: none;
    stroke: var(--accent);
    stroke-width: 2.4;
    stroke-linecap: round;
    stroke-linejoin: round;
  }
</style>
