<script lang="ts">
  // Clip library: thumbnails, lossless trim, delete (to the Recycle Bin) and the storage limit.
  import { onMount } from "svelte";
  import { flip } from "svelte/animate";
  import { fade, scale, slide } from "svelte/transition";
  import { backOut, cubicOut } from "svelte/easing";
  import { api, errorText, onClipsChanged, type ClipEntry, type Settings } from "./api";
  import Seg from "./Seg.svelte";
  import SliderField from "./SliderField.svelte";
  import { copyText, isTextField, showMenu } from "./menu-state.svelte";

  let { s, edit }: { s: Settings; edit: (f: (d: Settings) => void, delay?: number) => void } = $props();

  let clips = $state<ClipEntry[]>([]);
  let loaded = $state(false);
  let filter = $state<"all" | "video" | "image">("all");
  let error = $state("");
  let notice = $state("");
  /** The clip(s) the delete dialog is open for. */
  let confirming = $state<ClipEntry[]>([]);
  let selected = $state<Set<string>>(new Set());
  let anchor = $state<string | null>(null);
  let grid = $state<HTMLElement>();

  const shown = $derived(clips.filter((c) => filter === "all" || c.kind === filter));
  const totalBytes = $derived(clips.reduce((n, c) => n + c.size, 0));
  const selectedItems = $derived(shown.filter((c) => selected.has(c.path)));
  const selectedBytes = $derived(selectedItems.reduce((n, c) => n + c.size, 0));
  const allSelected = $derived(shown.length > 0 && selectedItems.length === shown.length);

  async function refresh() {
    try {
      clips = await api.clipsList();
      const valid = new Set(clips.map((c) => c.path));
      if ([...selected].some((p) => !valid.has(p))) {
        selected = new Set([...selected].filter((p) => valid.has(p)));
      }
      const validKeys = new Set(clips.map(key));
      for (const k of Object.keys(thumbs)) {
        if (!validKeys.has(k)) delete thumbs[k];
      }
      error = "";
    } catch (e) {
      error = errorText(e);
    }
    loaded = true;
  }

  async function run(f: () => Promise<unknown>) {
    try {
      await f();
      error = "";
    } catch (e) {
      error = errorText(e);
    }
  }

  const size = (b: number) => (b >= 1e9 ? `${(b / 1e9).toFixed(1)} GB` : `${Math.max(1, Math.round(b / 1e6))} MB`);
  const when = (t: number) => new Date(t * 1000).toLocaleString([], { dateStyle: "medium", timeStyle: "short" });
  const clock = (secs: number) => {
    const t = Math.max(0, secs);
    const m = Math.floor(t / 60);
    return `${m}:${(t - m * 60).toFixed(1).padStart(4, "0")}`;
  };
  /** "90", "1:30" or "1:30.5" to seconds. */
  function parseTime(text: string): number | null {
    const m = text.trim().match(/^(?:(\d+):)?(\d+(?:\.\d+)?)$/);
    return m ? +(m[1] ?? 0) * 60 + +m[2] : null;
  }

  // ---- Thumbnails: fetched when a card scrolls into view, bounded LRU-ish FIFO cache ----
  const MAX_THUMB_CACHE = 60;
  const thumbs = $state<Record<string, string | null>>({});
  const key = (c: ClipEntry) => `${c.path}@${c.modified}`;
  function lazyThumb(node: HTMLElement, c: ClipEntry) {
    const io = new IntersectionObserver(
      (entries) => {
        if (!entries[0].isIntersecting) return;
        const k = key(c);
        if (k in thumbs) return;
        const keys = Object.keys(thumbs);
        for (let i = 0; keys.length - i >= MAX_THUMB_CACHE; i++) {
          delete thumbs[keys[i]];
        }
        thumbs[k] = null;
        api
          .clipThumbnail(c.path)
          .then((b64) => {
            if (clips.some((x) => key(x) === k)) {
              thumbs[k] = b64 ? `data:image/png;base64,${b64}` : null;
            } else {
              delete thumbs[k];
            }
          })
          .catch(() => {});
      },
      { rootMargin: "200px" },
    );
    io.observe(node);
    return {
      update(next: ClipEntry) {
        c = next;
      },
      destroy: () => io.disconnect(),
    };
  }

  // ---- Opening: a cue on the card, and a short cooldown so repeat clicks cannot pile up players ----
  const OPEN_COOLDOWN_MS = 1500;
  let opening = $state<string | null>(null);
  let openTimer: ReturnType<typeof setTimeout> | undefined;
  function openClip(c: ClipEntry) {
    if (opening) return;
    opening = c.path;
    clearTimeout(openTimer);
    openTimer = setTimeout(() => (opening = null), OPEN_COOLDOWN_MS);
    run(() => api.clipOpen(c.path));
  }

  // ---- Trim ----
  let trimming = $state<string | null>(null);
  let trim = $state({ start: "0:00.0", end: "", length: null as number | null, busy: false });

  async function openTrim(c: ClipEntry) {
    if (trimming === c.path) {
      trimming = null;
      return;
    }
    trimming = c.path;
    trim = { start: "0:00.0", end: "", length: null, busy: false };
    const len = await api.clipDuration(c.path).catch(() => null);
    if (trimming !== c.path) return;
    trim.length = len;
    trim.end = len ? clock(len) : "";
  }

  async function doTrim(c: ClipEntry) {
    const a = parseTime(trim.start);
    const b = parseTime(trim.end);
    if (a === null || b === null) return void (error = "Type times like 90, 1:30 or 1:30.5.");
    if (b - a < 0.5) return void (error = "The end must be at least half a second after the start.");
    trim.busy = true;
    await run(async () => {
      const out = await api.clipTrim(c.path, a, Math.min(b, trim.length ?? b));
      notice = `Saved ${out.split("\\").pop()}`;
      trimming = null;
      await refresh();
    });
    trim.busy = false;
  }

  function toggleSelect(c: ClipEntry, shiftKey = false) {
    const next = new Set(selected);
    if (shiftKey && anchor) {
      const a = shown.findIndex((x) => x.path === anchor);
      const b = shown.findIndex((x) => x.path === c.path);
      if (a !== -1 && b !== -1) {
        const [lo, hi] = a < b ? [a, b] : [b, a];
        for (let i = lo; i <= hi; i++) next.add(shown[i].path);
        selected = next;
        anchor = c.path;
        return;
      }
    }
    if (next.has(c.path)) next.delete(c.path);
    else next.add(c.path);
    selected = next;
    anchor = c.path;
  }

  function selectAll() {
    selected = new Set(shown.map((c) => c.path));
  }

  function clearSelection() {
    selected = new Set();
    anchor = null;
  }

  function onThumbClick(e: MouseEvent, c: ClipEntry) {
    if (e.ctrlKey || e.metaKey || e.shiftKey) {
      e.preventDefault();
      toggleSelect(c, e.shiftKey);
    } else if (selected.size > 0) {
      toggleSelect(c);
    } else {
      openClip(c);
    }
  }

  async function delItems(items: ClipEntry[], permanent: boolean) {
    confirming = [];
    if (items.length === 0) return;
    const results = await Promise.allSettled(items.map((c) => api.clipDelete(c.path, permanent)));
    const nextSel = new Set(selected);
    let ok = 0;
    let firstErr = "";
    results.forEach((r, i) => {
      if (r.status === "fulfilled") {
        ok++;
        nextSel.delete(items[i].path);
        if (trimming === items[i].path) trimming = null;
      } else if (!firstErr) {
        firstErr = errorText(r.reason);
      }
    });
    selected = nextSel;
    if (ok > 0) {
      const what = items.length === 1 && ok === 1 ? items[0].name : `${ok} item${ok === 1 ? "" : "s"}`;
      notice = permanent ? `Deleted ${what} permanently` : `Moved ${what} to the Recycle Bin`;
    }
    error = firstErr;
    await refresh();
  }

  function menu(e: MouseEvent, c: ClipEntry) {
    if (selected.has(c.path) && selectedItems.length > 1) {
      const items = selectedItems;
      showMenu(e, [
        { header: `${items.length} selected · ${size(selectedBytes)}` },
        {
          label: `Open all (${items.length})`,
          disabled: items.length > 5 || opening !== null,
          hint: items.length > 5 ? "Max 5" : undefined,
          run: () => items.forEach((x) => run(() => api.clipOpen(x.path))),
        },
        { label: "Show in folder", run: () => run(() => api.clipReveal(items[0].path)) },
        { label: "Copy file names", run: () => copyText(items.map((x) => x.name).join("\n")) },
        { label: "Copy paths", run: () => copyText(items.map((x) => x.path).join("\n")) },
        { separator: true },
        { label: `Delete ${items.length} selected…`, danger: true, run: () => (confirming = [...items]) },
        { separator: true },
        ...(!allSelected ? [{ label: `Select all (${shown.length})`, hint: "Ctrl+A", run: selectAll }] : []),
        { label: "Clear selection", hint: "Esc", run: clearSelection },
      ]);
      return;
    }
    showMenu(e, [
      { label: c.kind === "video" ? "Play" : "Open", disabled: opening !== null, run: () => openClip(c) },
      { label: "Show in folder", run: () => run(() => api.clipReveal(c.path)) },
      { label: "Copy path", run: () => copyText(c.path) },
      { label: "Trim…", disabled: c.kind !== "video", run: () => openTrim(c) },
      { separator: true },
      { label: selected.has(c.path) ? "Deselect" : "Select", run: () => toggleSelect(c) },
      { label: `Select all (${shown.length})`, disabled: shown.length === 0, hint: "Ctrl+A", run: selectAll },
      { separator: true },
      { label: "Delete…", danger: true, run: () => (confirming = [c]) },
    ]);
  }

  /** Render the dialog on <body>, clear of the page's own transforms. */
  function portal(node: HTMLElement) {
    document.body.appendChild(node);
    return { destroy: () => node.remove() };
  }
  function focusFirst(node: HTMLElement) {
    requestAnimationFrame(() => node.focus({ preventScroll: true }));
  }
  function onKey(e: KeyboardEvent) {
    if (confirming.length > 0 && e.key === "Escape") {
      e.preventDefault();
      confirming = [];
      return;
    }
    if (grid?.offsetParent === null || isTextField(e.target)) return;
    if (e.key === "Escape" && selected.size > 0) {
      e.preventDefault();
      clearSelection();
    } else if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "a" && shown.length > 0) {
      e.preventDefault();
      selectAll();
    }
  }

  onMount(() => {
    refresh();
    const un = onClipsChanged(refresh);
    return () => {
      un.then((f) => f());
    };
  });
</script>

<svelte:window onkeydown={onKey} />

{#if confirming.length > 0}
  {@const items = confirming}
  {@const single = items.length === 1 ? items[0] : null}
  {@const totalDelBytes = items.reduce((n, x) => n + x.size, 0)}
  <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
  <div class="scrim" use:portal transition:fade={{ duration: 160 }} onclick={(e) => e.target === e.currentTarget && (confirming = [])}>
    <div class="modal" role="alertdialog" aria-modal="true" aria-labelledby="del-title" aria-describedby="del-desc" transition:scale={{ duration: 220, start: 0.94, easing: backOut }}>
      {#if single}
        <div class="modal-body">
          {#if thumbs[key(single)]}
            <img class="modal-thumb" src={thumbs[key(single)]} alt="" draggable="false" />
          {/if}
          <div class="modal-text">
            <h3 id="del-title">Delete this {single.kind === "video" ? "clip" : "screenshot"}?</h3>
            <div class="modal-name" title={single.name}>{single.name}</div>
            <p id="del-desc">{size(single.size)} · {when(single.modified)}</p>
          </div>
        </div>
      {:else}
        <div class="modal-text">
          <h3 id="del-title">Delete {items.length} items?</h3>
          <div class="modal-name">
            {items.slice(0, 3).map((x) => x.name).join(", ")}{items.length > 3 ? `, and ${items.length - 3} more` : ""}
          </div>
          <p id="del-desc">{size(totalDelBytes)} total</p>
        </div>
      {/if}
      <p class="modal-note">
        {single ? "The Recycle Bin keeps it until you empty it." : "The Recycle Bin keeps them until you empty it."} Deleting permanently cannot be undone.
      </p>
      <div class="modal-actions">
        <button class="btn primary" use:focusFirst onclick={() => delItems(items, false)}>
          {single ? "Move to Recycle Bin" : `Move ${items.length} to Recycle Bin`}
        </button>
        <button class="btn danger" onclick={() => delItems(items, true)}>
          {single ? "Delete permanently" : `Delete ${items.length} permanently`}
        </button>
        <button class="btn ghost cancel" onclick={() => (confirming = [])}>Cancel</button>
      </div>
    </div>
  </div>
{/if}

{#if error}
  <p class="banner warn" role="alert" transition:slide={{ duration: 220, easing: cubicOut }}>{error}</p>
{/if}
{#if notice}
  <p class="banner info" transition:slide={{ duration: 220, easing: cubicOut }}>
    {notice}
    <button class="link" onclick={() => (notice = "")}>Dismiss</button>
  </p>
{/if}

<h2>Storage</h2>
<section class="card">
  <div class="row">
    <div class="row-text">
      <div class="row-title">Storage limit</div>
      <div class="row-desc">
        {#if s.clips_quota_gb}
          Past {s.clips_quota_gb} GB the oldest clips are deleted for good. Clips from the last 2 minutes are always kept.
        {:else}
          Off. Clips stay until you delete them. Using {size(totalBytes)} now.
        {/if}
      </div>
    </div>
    <SliderField
      label="Storage limit in GB"
      min={0}
      max={500}
      step={5}
      hardMax={100000}
      defaultValue={0}
      presets={[0, 50, 100, 250, 500]}
      value={s.clips_quota_gb}
      format={(v) => (v === 0 ? "Off" : `${v} GB`)}
      onchange={(v) => edit((d) => (d.clips_quota_gb = Math.round(v)), 400)}
    />
  </div>
</section>

<div class="head">
  <h2>Library <span class="count">{shown.length} item{shown.length === 1 ? "" : "s"} · {size(totalBytes)}</span></h2>
  <div class="head-tools">
    <Seg
      label="Show"
      options={[
        { value: "all", label: "All" },
        { value: "video", label: "Clips" },
        { value: "image", label: "Screenshots" },
      ]}
      value={filter}
      onselect={(v) => (filter = v)}
    />
    <button class="btn ghost" onclick={() => run(api.openClipsFolder)}>Open folder</button>
  </div>
</div>

{#if selectedItems.length > 0}
  <div class="sel-bar" transition:slide={{ duration: 180, easing: cubicOut }}>
    <span class="sel-info"><b>{selectedItems.length}</b> selected · {size(selectedBytes)}</span>
    <div class="sel-acts">
      {#if !allSelected}
        <button class="btn ghost" onclick={selectAll}>Select all ({shown.length})</button>
      {/if}
      <button class="btn danger" onclick={() => (confirming = [...selectedItems])}>Delete {selectedItems.length}…</button>
      <button class="btn ghost" onclick={clearSelection}>Clear</button>
    </div>
  </div>
{/if}

{#if loaded && shown.length === 0}
  <section class="card empty">
    {#if clips.length === 0}
      Nothing here yet. Save a clip with a shortcut, or take a screenshot (Ctrl+Alt+F8).
    {:else}
      No {filter === "video" ? "clips" : "screenshots"} yet.
    {/if}
  </section>
{/if}

<div class="grid" class:selecting={selectedItems.length > 0} bind:this={grid}>
  {#each shown as c (c.path)}
    {@const isSel = selected.has(c.path)}
    <!-- svelte-ignore a11y_no_static_element_interactions -->
    <article class="clip" class:selected={isSel} animate:flip={{ duration: 260, easing: cubicOut }} transition:fade={{ duration: 160 }} oncontextmenu={(e) => menu(e, c)}>
      <div class="thumb-wrap">
        <button class="thumb" class:opening={opening === c.path} aria-busy={opening === c.path} use:lazyThumb={c} onclick={(e) => onThumbClick(e, c)} aria-label={selected.size > 0 ? `Toggle selection for ${c.name}` : `Open ${c.name}`}>
          {#if thumbs[key(c)]}
            <img src={thumbs[key(c)]} alt="" draggable="false" />
          {:else}
            <svg viewBox="0 0 24 24" aria-hidden="true">
              {#if c.kind === "video"}
                <path d="M5 4l14 8-14 8z" />
              {:else}
                <path d="M4 5h16v14H4zM4 16l5-5 4 4 3-3 4 4" />
              {/if}
            </svg>
          {/if}
          <span class="kind">{c.name.split(".").pop()}</span>
          {#if opening === c.path}
            <span class="opening-cue" transition:fade={{ duration: 140 }}><i class="spin"></i>Opening…</span>
          {/if}
        </button>
        <button
          class="pick"
          class:on={isSel}
          type="button"
          aria-pressed={isSel}
          aria-label={isSel ? `Deselect ${c.name}` : `Select ${c.name}`}
          onclick={(e) => {
            e.stopPropagation();
            toggleSelect(c, e.shiftKey);
          }}
        >
          {#if isSel}
            <svg viewBox="0 0 12 12" aria-hidden="true"><path d="M2.5 6.2l2.3 2.3L9.5 3.5" /></svg>
          {/if}
        </button>
      </div>
      <div class="meta">
        <div class="name" title={c.name}>{c.name}</div>
        <div class="sub">{c.app || "SuiCap"} · {size(c.size)} · {when(c.modified)}</div>
      </div>
      <div class="actions">
        <button class="btn ghost" onclick={() => run(() => api.clipReveal(c.path))}>Show</button>
        {#if c.kind === "video"}
          <button class="btn ghost" class:active={trimming === c.path} onclick={() => openTrim(c)}>Trim</button>
        {/if}
        <button class="btn ghost del" onclick={() => (confirming = [c])}>Delete</button>
      </div>
      {#if trimming === c.path}
        <div class="trim" transition:slide={{ duration: 220, easing: cubicOut }}>
          <label>Start <input bind:value={trim.start} spellcheck="false" inputmode="decimal" /></label>
          <label>End <input bind:value={trim.end} spellcheck="false" inputmode="decimal" /></label>
          <button class="btn primary" disabled={trim.busy} onclick={() => doTrim(c)}>{trim.busy ? "Saving…" : "Save trimmed copy"}</button>
          <p class="hint">
            {trim.length ? `Length ${clock(trim.length)}. ` : ""}Lossless: the cut starts at the nearest keyframe before your start (up to a second or two early). The original is kept.
          </p>
        </div>
      {/if}
    </article>
  {/each}
</div>

<style>
  .head {
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    gap: 12px;
    margin-top: 6px;
  }
  .head h2 {
    margin-bottom: 8px;
  }
  .head-tools {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 8px;
  }
  .count {
    margin-left: 6px;
    color: var(--text-3);
    font-weight: 500;
    text-transform: none;
    letter-spacing: 0;
  }
  h2 {
    margin: 20px 0 8px 4px;
    color: var(--text-3);
    font: 600 12px var(--font);
    letter-spacing: 0.04em;
    text-transform: uppercase;
  }
  .card {
    margin-bottom: 14px;
    border-radius: var(--radius);
    background: var(--surface);
    box-shadow: inset 0 0 0 1px var(--line);
  }
  .card.empty {
    padding: 28px 22px;
    color: var(--text-3);
    text-align: center;
  }
  .row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 24px;
    padding: 14px 18px;
    min-height: 60px;
  }
  .row-text {
    flex: 1;
    min-width: 0;
  }
  .row-title {
    font-size: 13.5px;
    font-weight: 550;
  }
  .row-desc {
    margin-top: 2px;
    color: var(--text-3);
    font-size: 12px;
    font-weight: 500;
  }
  .banner {
    display: flex;
    justify-content: space-between;
    gap: 12px;
    margin: 0 0 12px;
    padding: 10px 14px;
    border-radius: var(--radius-sm);
    font-size: 13px;
  }
  .banner.warn {
    background: var(--warn-soft);
    color: var(--warn);
  }
  .banner.info {
    background: var(--accent-soft);
  }
  .link {
    border: 0;
    background: none;
    color: var(--text-2);
    font-size: 12.5px;
    cursor: pointer;
  }

  .sel-bar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    margin-bottom: 12px;
    padding: 8px 12px;
    border-radius: var(--radius-sm);
    background: var(--accent-soft);
    box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--accent) 35%, transparent);
  }
  .sel-info {
    font-size: 12.5px;
    color: var(--text-2);
  }
  .sel-info b {
    color: var(--text);
    font-weight: 650;
  }
  .sel-acts {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .sel-acts .btn {
    height: 28px;
    padding: 0 10px;
  }
  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(230px, 1fr));
    gap: 12px;
    align-items: start;
  }
  .clip {
    overflow: hidden;
    border-radius: var(--radius);
    background: var(--surface);
    box-shadow: inset 0 0 0 1px var(--line);
    content-visibility: auto;
    contain-intrinsic-size: auto 220px;
    transition: box-shadow var(--fast) var(--ease);
  }
  .clip.selected {
    box-shadow: inset 0 0 0 2px var(--accent);
  }
  .thumb-wrap {
    position: relative;
  }
  .pick {
    position: absolute;
    top: 8px;
    left: 8px;
    display: grid;
    place-items: center;
    width: 20px;
    height: 20px;
    padding: 0;
    border: 0;
    border-radius: 6px;
    background: rgb(0 0 0 / 0.55);
    box-shadow: inset 0 0 0 1.5px rgb(255 255 255 / 0.7);
    color: var(--on-accent);
    opacity: 0;
    cursor: pointer;
    transition:
      opacity var(--fast) var(--ease),
      background var(--fast) var(--ease),
      box-shadow var(--fast) var(--ease),
      transform var(--med) var(--spring);
  }
  .clip:hover .pick,
  .grid.selecting .pick,
  .pick.on,
  .pick:focus-visible {
    opacity: 1;
  }
  .pick.on {
    background: var(--accent);
    box-shadow: inset 0 0 0 1.5px var(--accent);
  }
  .pick:active {
    transform: scale(0.9);
  }
  .pick svg {
    width: 12px;
    height: 12px;
    fill: none;
    stroke: currentColor;
    stroke-width: 2.2;
    stroke-linecap: round;
    stroke-linejoin: round;
  }
  .thumb {
    position: relative;
    display: grid;
    place-items: center;
    width: 100%;
    aspect-ratio: 16 / 9;
    padding: 0;
    border: 0;
    background: var(--surface-3);
    color: var(--text-3);
    cursor: pointer;
    overflow: hidden;
  }
  .thumb img {
    width: 100%;
    height: 100%;
    object-fit: cover;
    transition: transform var(--med) var(--ease);
  }
  .thumb:hover img {
    transform: scale(1.04);
  }
  .thumb:active img {
    transform: scale(1.01);
  }
  .thumb.opening {
    cursor: progress;
  }
  .thumb.opening img {
    transform: scale(1);
    filter: brightness(0.6);
  }
  .thumb img {
    transition:
      transform var(--med) var(--ease),
      filter var(--med) var(--ease);
  }
  .opening-cue {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 8px;
    color: #fff;
    font-size: 12.5px;
    font-weight: 600;
    text-shadow: 0 1px 6px rgb(0 0 0 / 0.5);
  }
  .spin {
    width: 14px;
    height: 14px;
    border: 2px solid rgb(255 255 255 / 0.35);
    border-top-color: #fff;
    border-radius: 50%;
    animation: spin 0.7s linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .spin {
      animation-duration: 2s;
    }
    .thumb img {
      transition: none;
    }
  }
  .thumb svg {
    width: 30px;
    height: 30px;
    fill: none;
    stroke: currentColor;
    stroke-width: 1.8;
    stroke-linejoin: round;
  }
  .kind {
    position: absolute;
    right: 8px;
    bottom: 8px;
    padding: 2px 7px;
    border-radius: 6px;
    background: rgb(0 0 0 / 0.6);
    color: #fff;
    font-size: 10.5px;
    font-weight: 700;
    letter-spacing: 0.04em;
    text-transform: uppercase;
  }
  .meta {
    padding: 10px 12px 4px;
  }
  .name {
    overflow: hidden;
    font-size: 13px;
    font-weight: 600;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .sub {
    margin-top: 2px;
    color: var(--text-3);
    font-size: 11.5px;
  }
  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: 2px;
    padding: 4px 8px 8px;
  }
  .actions .btn {
    height: 28px;
    padding: 0 10px;
  }
  .btn {
    border: 0;
    border-radius: var(--radius-sm);
    background: var(--surface-3);
    font-size: 12.5px;
    font-weight: 600;
    cursor: pointer;
    transition:
      background var(--fast) var(--ease),
      transform var(--med) var(--spring);
  }
  .btn:active:not(:disabled) {
    transform: scale(0.96);
  }
  .btn:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .btn.ghost {
    background: transparent;
    color: var(--text-2);
  }
  .btn.ghost:hover,
  .btn.ghost.active {
    background: var(--surface-2);
    color: var(--text);
  }
  .btn.primary {
    height: 30px;
    padding: 0 12px;
    background: var(--accent);
    color: var(--on-accent);
  }
  .btn.danger {
    background: var(--warn-soft);
    color: var(--warn);
  }
  .btn.danger:hover:not(:disabled) {
    background: color-mix(in srgb, var(--warn) 28%, transparent);
  }
  .actions .del:hover {
    color: var(--warn);
  }

  /* ---- Delete dialog ---- */
  .scrim {
    position: fixed;
    inset: 0;
    z-index: 50;
    display: grid;
    place-items: center;
    padding: 24px;
    background: rgb(0 0 0 / 0.55);
    backdrop-filter: blur(2px);
  }
  .modal {
    width: min(420px, 100%);
    padding: 20px;
    border-radius: 16px;
    background: var(--surface-solid);
    box-shadow:
      inset 0 0 0 1px var(--line-strong),
      0 24px 60px rgb(0 0 0 / 0.5);
  }
  .modal-body {
    display: flex;
    align-items: center;
    gap: 14px;
  }
  .modal-thumb {
    width: 112px;
    aspect-ratio: 16 / 9;
    flex: none;
    border-radius: 8px;
    object-fit: cover;
    box-shadow: inset 0 0 0 1px var(--line);
  }
  .modal-text {
    min-width: 0;
  }
  .modal h3 {
    margin: 0;
    font: 650 16px var(--font-display);
  }
  .modal-name {
    overflow: hidden;
    margin-top: 4px;
    font-size: 12.5px;
    font-weight: 600;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .modal-text p {
    margin: 2px 0 0;
    color: var(--text-3);
    font-size: 12px;
  }
  .modal-note {
    margin: 14px 0 16px;
    color: var(--text-2);
    font-size: 12.5px;
    line-height: 1.45;
  }
  .modal-actions {
    display: grid;
    gap: 8px;
  }
  .modal-actions .btn {
    height: 36px;
    padding: 0 14px;
  }
  .modal-actions .cancel {
    height: 32px;
  }
  .trim {
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
    gap: 8px;
    padding: 4px 12px 12px;
  }
  .trim label {
    display: grid;
    min-width: 0;
    gap: 4px;
    color: var(--text-3);
    font-size: 11.5px;
  }
  .trim input {
    width: 100%;
    min-width: 0;
    box-sizing: border-box;
    height: 30px;
    padding: 0 8px;
    border: 0;
    border-radius: var(--radius-sm);
    background: var(--surface-3);
    color: var(--text);
    font: 500 13px var(--font);
    font-variant-numeric: tabular-nums;
  }
  .trim .btn.primary {
    grid-column: 1 / -1;
    min-width: 0;
  }
  .hint {
    grid-column: 1 / -1;
    min-width: 0;
    margin: 0;
    color: var(--text-3);
    font-size: 11.5px;
    line-height: 1.35;
  }
</style>
