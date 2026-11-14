<script lang="ts">
  // The menu itself: spring-in, clamped to the window, arrow keys / Enter / Escape, closes on
  // outside click, scroll, resize or focus loss. Also gives text fields Cut / Copy / Paste.
  import { tick } from "svelte";
  import { fade, scale } from "svelte/transition";
  import { backOut } from "svelte/easing";
  import AppIcon from "./AppIcon.svelte";
  import { friendlyName } from "./icons";
  import { closeMenu, copyText, isTextField, menu, showMenu, type MenuItem } from "./menu-state.svelte";

  let el = $state<HTMLDivElement>();
  let active = $state(-1);
  let pos = $state({ x: 0, y: 0, ox: "0%", oy: "0%" });

  const actionable = (it: MenuItem) => "label" in it && !it.disabled;
  const rows = $derived(menu.items.map((it, i) => ({ it, i })).filter((r) => actionable(r.it)));

  // Place the menu once it is measured: flip to the other side of the pointer near an edge.
  $effect(() => {
    if (!menu.open) return;
    const { x, y } = menu;
    void menu.id;
    active = -1;
    tick().then(() => {
      if (!el) return;
      const w = el.offsetWidth;
      const h = el.offsetHeight;
      const pad = 8;
      const left = x + w + pad > innerWidth;
      const up = y + h + pad > innerHeight;
      pos = {
        x: Math.max(pad, left ? x - w : x),
        y: Math.max(pad, up ? Math.min(y - h, innerHeight - h - pad) : y),
        ox: left ? "100%" : "0%",
        oy: up ? "100%" : "0%",
      };
      el.focus({ preventScroll: true });
    });
  });

  function step(dir: number) {
    if (!rows.length) return;
    const cur = rows.findIndex((r) => r.i === active);
    const next = cur < 0 ? (dir > 0 ? 0 : rows.length - 1) : (cur + dir + rows.length) % rows.length;
    active = rows[next].i;
  }

  function run(it: MenuItem) {
    if (!("label" in it) || it.disabled) return;
    closeMenu();
    void it.run();
  }

  function onKey(e: KeyboardEvent) {
    if (!menu.open) return;
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      closeMenu();
    } else if (e.key === "ArrowDown") (e.preventDefault(), step(1));
    else if (e.key === "ArrowUp") (e.preventDefault(), step(-1));
    else if (e.key === "Home") (e.preventDefault(), (active = rows[0]?.i ?? -1));
    else if (e.key === "End") (e.preventDefault(), (active = rows[rows.length - 1]?.i ?? -1));
    else if ((e.key === "Enter" || e.key === " ") && active >= 0) {
      e.preventDefault();
      run(menu.items[active]);
    } else if (e.key === "Tab") closeMenu();
  }

  function outside(e: Event) {
    if (menu.open && el && !el.contains(e.target as Node)) closeMenu();
  }

  // ---- Text fields: the usual edit menu, since the browser's is blocked ----
  function textMenu(e: MouseEvent) {
    const t = e.target;
    if (!isTextField(t)) return;
    const hasSel = (t.selectionStart ?? 0) !== (t.selectionEnd ?? 0);
    const selected = () => t.value.slice(t.selectionStart ?? 0, t.selectionEnd ?? 0);
    const put = (text: string) => {
      t.setRangeText(text, t.selectionStart ?? t.value.length, t.selectionEnd ?? t.value.length, "end");
      t.dispatchEvent(new Event("input", { bubbles: true }));
    };
    t.focus();
    showMenu(e, [
      {
        label: "Cut",
        hint: "Ctrl+X",
        disabled: !hasSel || t.readOnly,
        run: async () => {
          await copyText(selected());
          put("");
        },
      },
      { label: "Copy", hint: "Ctrl+C", disabled: !hasSel, run: () => copyText(selected()) },
      {
        label: "Paste",
        hint: "Ctrl+V",
        disabled: t.readOnly,
        run: async () => {
          try {
            put(await navigator.clipboard.readText());
          } catch {
            /* clipboard not readable: nothing to paste */
          }
        },
      },
      { separator: true },
      { label: "Select all", hint: "Ctrl+A", run: () => t.select() },
    ], { text: true });
  }
</script>

<svelte:window
  oncontextmenu={textMenu}
  onkeydowncapture={onKey}
  onpointerdowncapture={outside}
  onblur={closeMenu}
  onresize={closeMenu}
  onwheel={closeMenu}
/>

{#if menu.open}
  <div
    class="ctx"
    bind:this={el}
    role="menu"
    tabindex="-1"
    aria-label="Context menu"
    style="left: {pos.x}px; top: {pos.y}px; transform-origin: {pos.ox} {pos.oy}"
    in:scale={{ duration: 240, start: 0.92, easing: backOut }}
    out:fade={{ duration: 90 }}
    oncontextmenu={(e) => e.preventDefault()}
  >
    {#if menu.app}
      <div class="who">
        <AppIcon exe={menu.app} size={20} />
        <span>{friendlyName(menu.app)}</span>
        <small>{menu.app}</small>
      </div>
    {/if}
    {#each menu.items as it, i (i)}
      {#if "separator" in it}
        <div class="sep" role="separator"></div>
      {:else if "header" in it}
        <div class="head">{it.header}</div>
      {:else}
        <button
          role="menuitem"
          class="item"
          class:danger={it.danger}
          class:active={active === i}
          disabled={it.disabled}
          onmouseenter={() => (active = it.disabled ? -1 : i)}
          onclick={() => run(it)}
        >
          <span class="lead">
            {#if it.dot}<i class="dot" style="background: {it.dot}"></i>{/if}
          </span>
          <span class="label">{it.label}</span>
          {#if it.checked}<svg class="tick" viewBox="0 0 24 24" aria-hidden="true"><path d="M5 12.5l4.5 4.5L19 7.5" /></svg>{/if}
          {#if it.hint}<span class="hint">{it.hint}</span>{/if}
        </button>
      {/if}
    {/each}
  </div>
{/if}

<style>
  .ctx {
    position: fixed;
    z-index: 1000;
    min-width: 210px;
    max-width: 300px;
    padding: 5px;
    border-radius: 13px;
    outline: 0;
    background: var(--surface-solid);
    box-shadow:
      inset 0 0 0 1px var(--line-strong),
      var(--shadow-menu);
    font-size: 12.5px;
  }
  .who {
    display: flex;
    align-items: center;
    gap: 9px;
    padding: 7px 9px 8px;
    font-weight: 650;
  }
  .who small {
    margin-left: auto;
    overflow: hidden;
    max-width: 110px;
    color: var(--text-3);
    font-size: 11px;
    font-weight: 500;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .head {
    padding: 6px 10px 3px;
    color: var(--text-3);
    font-size: 11px;
    font-weight: 650;
    letter-spacing: 0.04em;
    text-transform: uppercase;
  }
  .sep {
    height: 1px;
    margin: 5px 8px;
    background: var(--line);
  }
  .item {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    padding: 6px 9px;
    border: 0;
    border-radius: 9px;
    background: transparent;
    font-weight: 500;
    text-align: left;
    cursor: default;
    transition:
      background var(--fast) var(--ease),
      transform var(--med) var(--spring);
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
  .item.danger {
    color: var(--rec);
  }
  .item.danger.active {
    background: var(--rec-soft);
  }
  .lead {
    display: grid;
    flex: none;
    place-items: center;
    width: 10px;
  }
  .lead:empty {
    display: none;
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
  }
  .label {
    flex: 1;
  }
  .hint {
    color: var(--text-3);
    font-size: 11.5px;
    font-variant-numeric: tabular-nums;
  }
  .tick {
    width: 14px;
    height: 14px;
    fill: none;
    stroke: var(--accent);
    stroke-width: 2.4;
    stroke-linecap: round;
    stroke-linejoin: round;
  }
</style>
