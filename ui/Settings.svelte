<script lang="ts">
  import { onMount } from "svelte";
  import { api, errorText, onStatus, type Display, type PresetName, type Settings, type Status } from "./api";
  import HotkeyInput from "./HotkeyInput.svelte";
  import AppList from "./AppList.svelte";

  let { initial }: { initial: Settings } = $props();

  // The page owns its draft after the first load.
  // svelte-ignore state_referenced_locally
  let s = $state<Settings>(structuredClone(initial));
  let status = $state<Status | null>(null);
  let displays = $state<Display[]>([]);
  let defaultDir = $state("");
  let estimate = $state(0);
  let saveError = $state("");
  let actionError = $state("");
  let page = $state<"replay" | "video" | "audio" | "shortcuts" | "app">("replay");

  const pages = [
    { id: "replay", label: "Replay" },
    { id: "video", label: "Video" },
    { id: "audio", label: "Audio" },
    { id: "shortcuts", label: "Shortcuts" },
    { id: "app", label: "App" },
  ] as const;

  const presets: { id: PresetName; name: string; desc: string }[] = [
    { id: "compat", name: "Compatible", desc: "H.264 1080p60. Plays on any device and site." },
    { id: "balanced", name: "Balanced", desc: "Best codec your GPU has, 1080p60." },
    { id: "high", name: "High", desc: "Your monitor's own resolution, 60 fps, higher quality." },
    { id: "custom", name: "Custom", desc: "Pick resolution, frame rate, codec and quality." },
  ];

  const resolutions = [
    { w: 0, h: 0, label: "Monitor size" },
    { w: 1280, h: 720, label: "1280 × 720" },
    { w: 1920, h: 1080, label: "1920 × 1080" },
    { w: 2560, h: 1440, label: "2560 × 1440" },
    { w: 3840, h: 2160, label: "3840 × 2160" },
  ];

  const armed = $derived(status?.phase === "armed");
  const running = $derived(
    status?.phase === "armed" || status?.phase === "starting" || status?.phase === "recovering",
  );
  const fill = $derived(status && status.buffer_secs ? Math.min(1, status.buffered_secs / status.buffer_secs) : 0);

  // Last settings the backend accepted; "Undo" returns to them.
  // svelte-ignore state_referenced_locally
  let saved = structuredClone(initial);
  /** Edits that would restart capture and clear a filled buffer wait for "Apply" (plan §7). */
  let pending = $state(false);

  /** The settings that restart capture when changed (mirrors the shell's capture_key). */
  function captureKey(x: Settings): string {
    const { preset, custom, buffer_secs, container, clips_dir, cursor, display, audio } = x;
    return JSON.stringify({ preset, custom, buffer_secs, container, clips_dir, cursor, display, audio });
  }

  let timer: ReturnType<typeof setTimeout> | undefined;
  /** Apply an edit and save it (debounced for sliders and typing). */
  function edit(f: (d: Settings) => void, delay = 0) {
    f(s);
    clearTimeout(timer);
    const restarts = captureKey($state.snapshot(s) as Settings) !== captureKey(saved);
    if (pending || (restarts && running && (status?.buffered_secs ?? 0) >= 1)) {
      pending = restarts || pending;
      if (pending) return;
    }
    timer = setTimeout(persist, delay);
  }

  function undo() {
    s = structuredClone(saved);
    pending = false;
  }

  async function persist() {
    pending = false;
    const draft = $state.snapshot(s) as Settings;
    try {
      status = await api.configSet(draft);
      saved = structuredClone(draft);
      saveError = "";
    } catch (e) {
      saveError = errorText(e);
    }
    estimate = await api.estimateMb(draft).catch(() => 0);
  }

  async function act(f: () => Promise<unknown>) {
    try {
      await f();
      actionError = "";
    } catch (e) {
      actionError = errorText(e);
    }
  }

  async function pickDir() {
    const dir = await api.pickClipsDir().catch((e) => ((actionError = errorText(e)), null));
    if (dir) edit((d) => (d.clips_dir = dir));
  }

  function phaseText(st: Status | null): string {
    switch (st?.phase) {
      case "armed":
        return "Replay on";
      case "starting":
        return "Starting capture…";
      case "recovering":
        return "Restarting capture…";
      case "error":
        return "Capture stopped";
      default:
        return "Replay off";
    }
  }

  onMount(() => {
    const un = onStatus((st) => (status = st));
    api.status().then((st) => (status = st));
    api.displays().then((d) => (displays = d));
    api.defaultClipsDir().then((d) => (defaultDir = d));
    api.estimateMb($state.snapshot(s) as Settings).then((m) => (estimate = m));
    return () => {
      un.then((f) => f());
    };
  });
</script>

{#snippet toggle(on: boolean, set: (v: boolean) => void, label: string)}
  <button class="switch" class:on role="switch" aria-checked={on} aria-label={label} onclick={() => set(!on)}>
    <span></span>
  </button>
{/snippet}

{#snippet row(title: string, desc: string)}
  <div class="row-text">
    <div class="row-title">{title}</div>
    {#if desc}<div class="row-desc">{desc}</div>{/if}
  </div>
{/snippet}

<div class="shell">
  <aside>
    <div class="brand">
      <svg viewBox="0 0 512 512" aria-hidden="true">
        <path
          d="M256 112a144 144 0 1 1-136 96"
          fill="none"
          stroke="currentColor"
          stroke-width="44"
          stroke-linecap="round"
        />
        <path d="M86 186l42 54 52-46z" fill="currentColor" />
        <circle cx="256" cy="256" r="70" fill="var(--accent)" />
      </svg>
      SuiCap
    </div>
    <nav>
      {#each pages as p (p.id)}
        <button class:current={page === p.id} aria-current={page === p.id ? "page" : undefined} onclick={() => (page = p.id)}>
          {p.label}
        </button>
      {/each}
    </nav>
    <div class="mini" class:on={armed} class:err={status?.phase === "error"}>
      <div class="mini-head"><span class="dot"></span>{phaseText(status)}</div>
      <div class="meter"><span style="width: {fill * 100}%"></span></div>
    </div>
  </aside>

  <main>
    {#if status?.settings_warning}
      <p class="banner warn">{status.settings_warning}</p>
    {/if}
    {#if pending}
      <div class="banner confirm" role="alert">
        <span>These changes restart capture and clear the {Math.round(status?.buffered_secs ?? 0)} s in the replay buffer.</span>
        <span class="inline">
          <button class="btn ghost" onclick={undo}>Undo</button>
          <button class="btn primary" onclick={persist}>Apply</button>
        </span>
      </div>
    {/if}
    {#each status?.notes ?? [] as note (note)}
      <p class="banner info">{note}</p>
    {/each}
    {#if saveError}
      <p class="banner warn" role="alert">Not saved: {saveError}</p>
    {/if}
    {#if actionError}
      <p class="banner warn" role="alert">{actionError}</p>
    {/if}

    {#if page === "replay"}
      <h1>Replay</h1>
      <section class="card status" class:on={armed} class:err={status?.phase === "error"}>
        <div class="status-top">
          <div>
            <div class="status-phase"><span class="dot"></span>{phaseText(status)}</div>
            <div class="status-detail">
              {#if armed && status}
                {status.encoder} · {status.resolution}{status.dropped ? ` · ${status.dropped} dropped frames` : ""}
              {:else if status?.phase === "error" || status?.phase === "recovering"}
                {status.error}
              {:else if !running}
                Turn replay on to keep the last {s.buffer_secs} seconds in memory.
              {/if}
            </div>
          </div>
          {#if running}
            <button class="btn" onclick={() => act(api.disarm)}>Turn off</button>
          {:else}
            <button class="btn primary" onclick={() => act(api.arm)}>Turn on</button>
          {/if}
        </div>
        <div class="meter big"><span style="width: {fill * 100}%"></span></div>
        <div class="meter-legend">
          <span
            >{status ? Math.min(status.buffered_secs, status.buffer_secs).toFixed(0) : 0} of {status?.buffer_secs ??
              s.buffer_secs} s buffered</span
          >
          {#if armed && status}<span>{status.buffered_mb.toFixed(0)} MB in memory</span>{/if}
        </div>
        <div class="save-row">
          <span class="label">Save</span>
          <button class="btn" disabled={!armed} onclick={() => act(() => api.saveClip(15))}>15 s</button>
          <button class="btn" disabled={!armed} onclick={() => act(() => api.saveClip(30))}>30 s</button>
          <button class="btn" disabled={!armed} onclick={() => act(() => api.saveClip(60))}>60 s</button>
          <button class="btn" disabled={!armed} onclick={() => act(() => api.saveClip(null))}>Whole buffer</button>
          <span class="spacer"></span>
          {#if status?.last_clip}
            <button class="btn ghost" onclick={() => act(api.revealLastClip)}>Show last clip</button>
          {/if}
          <button class="btn ghost" onclick={() => act(api.openClipsFolder)}>Open folder</button>
        </div>
      </section>

      <section class="card">
        <div class="row">
          {@render row("Buffer length", `Up to ${estimate} MB of memory at the maximum bitrate.`)}
          <div class="range">
            <input
              type="range"
              min="10"
              max="600"
              step="5"
              value={s.buffer_secs}
              aria-label="Buffer length in seconds"
              oninput={(e) => edit((d) => (d.buffer_secs = +e.currentTarget.value), 400)}
            />
            <output>{s.buffer_secs >= 60 ? `${Math.floor(s.buffer_secs / 60)} min ${s.buffer_secs % 60 ? `${s.buffer_secs % 60} s` : ""}` : `${s.buffer_secs} s`}</output>
          </div>
        </div>
        <div class="row">
          {@render row("File format", "MP4 plays everywhere. MKV stores audio as Opus.")}
          <div class="seg">
            {#each ["mp4", "mkv"] as const as c (c)}
              <button class:current={s.container === c} onclick={() => edit((d) => (d.container = c))}>{c.toUpperCase()}</button>
            {/each}
          </div>
        </div>
        <div class="row">
          {@render row("Clips folder", s.clips_dir ?? defaultDir)}
          <div class="inline">
            {#if s.clips_dir}
              <button class="btn ghost" onclick={() => edit((d) => (d.clips_dir = null))}>Use default</button>
            {/if}
            <button class="btn" onclick={pickDir}>Change…</button>
          </div>
        </div>
      </section>
    {:else if page === "video"}
      <h1>Video</h1>
      <div class="presets">
        {#each presets as p (p.id)}
          <button class="preset" class:current={s.preset === p.id} onclick={() => edit((d) => (d.preset = p.id))}>
            <span class="preset-name">{p.name}</span>
            <span class="preset-desc">{p.desc}</span>
          </button>
        {/each}
      </div>
      {#if s.preset === "custom"}
        <section class="card">
          <div class="row">
            {@render row("Resolution", "Video is scaled on the GPU and letterboxed to keep its shape.")}
            <select
              aria-label="Resolution"
              value={`${s.custom.width}x${s.custom.height}`}
              onchange={(e) => {
                const [w, h] = e.currentTarget.value.split("x").map(Number);
                edit((d) => ((d.custom.width = w), (d.custom.height = h)));
              }}
            >
              {#each resolutions as r (r.label)}<option value={`${r.w}x${r.h}`}>{r.label}</option>{/each}
            </select>
          </div>
          <div class="row">
            {@render row("Frame rate", "Capped at the monitor's refresh rate.")}
            <div class="seg">
              {#each [30, 60, 120, 144, 240] as f (f)}
                <button class:current={s.custom.fps === f} onclick={() => edit((d) => (d.custom.fps = f))}>{f}</button>
              {/each}
            </div>
          </div>
          <div class="row">
            {@render row("Codec", "Auto picks AV1, then HEVC, then H.264, by what your GPU supports.")}
            <div class="seg">
              {#each [null, "av1", "hevc", "h264"] as const as c (c ?? "auto")}
                <button class:current={s.custom.codec === c} onclick={() => edit((d) => (d.custom.codec = c))}>
                  {c === null ? "Auto" : c === "h264" ? "H.264" : c.toUpperCase()}
                </button>
              {/each}
            </div>
          </div>
          <div class="row">
            {@render row("Quality", "Lower is sharper and bigger. 23 suits most games.")}
            <div class="range">
              <input
                type="range"
                min="15"
                max="35"
                value={s.custom.quality}
                aria-label="Quality level"
                oninput={(e) => edit((d) => (d.custom.quality = +e.currentTarget.value), 400)}
              />
              <output>{s.custom.quality}</output>
            </div>
          </div>
        </section>
      {/if}
      <section class="card">
        <div class="row">
          {@render row("Monitor", "Which screen to record.")}
          <select
            aria-label="Monitor"
            value={s.display ?? 0}
            onchange={(e) => {
              const n = +e.currentTarget.value;
              edit((d) => (d.display = n || null));
            }}
          >
            <option value={0}>Main monitor</option>
            {#if s.display && !displays.some((d) => d.number === s.display)}
              <option value={s.display}>Display {s.display} (not connected)</option>
            {/if}
            {#each displays as d (d.number)}<option value={d.number}>{d.label}{d.primary ? " (main)" : ""}</option>{/each}
          </select>
        </div>
        <div class="row">
          {@render row("Show mouse pointer", "")}
          {@render toggle(s.cursor, (v) => edit((d) => (d.cursor = v)), "Show mouse pointer")}
        </div>
      </section>
    {:else if page === "audio"}
      <h1>Audio</h1>
      <section class="card">
        <div class="row">
          {@render row("Record audio", "Each source gets its own track, plus a mixed track that plays by default.")}
          {@render toggle(s.audio.enabled, (v) => edit((d) => (d.audio.enabled = v)), "Record audio")}
        </div>
        <div class="row" class:dim={!s.audio.enabled}>
          {@render row("Microphone", "Your default recording device.")}
          {@render toggle(s.audio.mic, (v) => edit((d) => (d.audio.mic = v)), "Microphone")}
        </div>
      </section>
      <section class="card" class:dim={!s.audio.enabled}>
        <div class="stack">
          {@render row("Game", "Leave empty to use the full-screen app in front when replay starts.")}
          <AppList items={s.audio.game} placeholder="game.exe" onchange={(v) => edit((d) => (d.audio.game = v))} />
        </div>
        <div class="stack">
          {@render row("Voice chat", "")}
          <AppList items={s.audio.voice} placeholder="discord.exe" onchange={(v) => edit((d) => (d.audio.voice = v))} />
        </div>
        <div class="stack">
          {@render row("Music and media", "")}
          <AppList items={s.audio.media} placeholder="spotify.exe" onchange={(v) => edit((d) => (d.audio.media = v))} />
        </div>
      </section>
    {:else if page === "shortcuts"}
      <h1>Shortcuts</h1>
      <p class="lead">These work in any app, including full-screen games.</p>
      {#if status?.hotkey_errors.length}
        <p class="banner warn">
          Another app already uses: {status.hotkey_errors.join("; ")}. Pick a different combination.
        </p>
      {/if}
      <section class="card">
        {#each [["save_15", "Save last 15 seconds"], ["save_30", "Save last 30 seconds"], ["save_60", "Save last 60 seconds"], ["save_full", "Save whole buffer"]] as const as [key, label] (key)}
          <div class="row">
            {@render row(label, "")}
            <HotkeyInput value={s.hotkeys[key]} onchange={(v) => edit((d) => (d.hotkeys[key] = v))} />
          </div>
        {/each}
      </section>
      <p class="foot">Click a shortcut, then press the new keys. Backspace clears it, Escape keeps the old one.</p>
    {:else}
      <h1>App</h1>
      <section class="card">
        <div class="row">
          {@render row("Sound when a clip is saved", "")}
          {@render toggle(s.save_sound, (v) => edit((d) => (d.save_sound = v)), "Sound when a clip is saved")}
        </div>
        <div class="row">
          {@render row("Notification when a clip is saved", "Windows hides these while a game is full screen.")}
          {@render toggle(s.save_toast, (v) => edit((d) => (d.save_toast = v)), "Notification when a clip is saved")}
        </div>
      </section>
      <section class="card">
        <div class="row">
          {@render row("Start with Windows", "Opens in the tray, without this window.")}
          {@render toggle(s.autostart, (v) => edit((d) => (d.autostart = v)), "Start with Windows")}
        </div>
        <div class="row">
          {@render row("Turn replay on at start", "")}
          {@render toggle(s.arm_on_start, (v) => edit((d) => (d.arm_on_start = v)), "Turn replay on at start")}
        </div>
        <div class="row">
          {@render row("Theme", "")}
          <div class="seg">
            {#each ["system", "light", "dark"] as const as t (t)}
              <button class:current={s.theme === t} onclick={() => edit((d) => (d.theme = t))}>
                {t[0].toUpperCase() + t.slice(1)}
              </button>
            {/each}
          </div>
        </div>
      </section>
      <p class="foot">Closing this window keeps SuiCap running in the tray. Quit from the tray menu.</p>
    {/if}
  </main>
</div>

<style>
  .shell {
    display: grid;
    grid-template-columns: 200px 1fr;
    height: 100vh;
  }

  aside {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 16px 10px;
    border-right: 1px solid var(--line);
    background: var(--surface);
  }

  .brand {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 2px 10px 16px;
    font: 600 15px var(--font-display);
    letter-spacing: 0.01em;
  }
  .brand svg {
    width: 22px;
    height: 22px;
    color: var(--text);
  }

  nav {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  nav button {
    position: relative;
    padding: 7px 12px;
    border: 0;
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--text-2);
    text-align: left;
    cursor: pointer;
  }
  nav button:hover {
    background: var(--surface-2);
    color: var(--text);
  }
  nav button.current {
    background: var(--surface-3);
    color: var(--text);
  }
  nav button.current::before {
    content: "";
    position: absolute;
    left: 0;
    top: 9px;
    bottom: 9px;
    width: 3px;
    border-radius: 2px;
    background: var(--accent);
  }

  .mini {
    margin-top: auto;
    padding: 10px 12px;
    border: 1px solid var(--line);
    border-radius: var(--radius);
    background: var(--surface-2);
    font-size: 12.5px;
    color: var(--text-2);
  }
  .mini-head {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 8px;
  }

  .dot {
    display: inline-block;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--text-3);
    flex: none;
  }
  .on .dot {
    background: var(--accent);
    box-shadow: 0 0 0 3px var(--accent-soft);
  }
  .err .dot {
    background: var(--warn);
  }

  .meter {
    height: 4px;
    border-radius: 2px;
    background: var(--surface-3);
    overflow: hidden;
  }
  .meter span {
    display: block;
    height: 100%;
    background: var(--text-3);
    transition: width 600ms linear;
  }
  .on .meter span {
    background: var(--accent);
  }
  .meter.big {
    height: 6px;
    margin-top: 16px;
  }

  main {
    overflow-y: auto;
    padding: 24px 32px 40px;
  }

  h1 {
    margin: 0 0 16px;
    font: 600 22px var(--font-display);
  }
  .lead {
    margin: -8px 0 16px;
    color: var(--text-2);
  }
  .foot {
    color: var(--text-3);
    font-size: 12.5px;
  }

  .card {
    margin-bottom: 14px;
    border: 1px solid var(--line);
    border-radius: var(--radius);
    background: var(--surface);
  }
  .card.dim,
  .row.dim {
    opacity: 0.5;
  }

  .status {
    padding: 18px 20px;
  }
  .status-top {
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    gap: 16px;
  }
  .status-phase {
    display: flex;
    align-items: center;
    gap: 10px;
    font: 600 17px var(--font-display);
  }
  .status-detail {
    margin-top: 4px;
    color: var(--text-2);
    font-size: 13px;
  }
  .meter-legend {
    display: flex;
    justify-content: space-between;
    margin-top: 6px;
    color: var(--text-3);
    font-size: 12px;
    font-variant-numeric: tabular-nums;
  }
  .save-row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px;
    margin-top: 16px;
    padding-top: 14px;
    border-top: 1px solid var(--line);
  }
  .save-row .label {
    margin-right: 4px;
    color: var(--text-2);
    font-size: 13px;
  }
  .spacer {
    flex: 1;
  }

  .row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 24px;
    padding: 14px 18px;
    min-height: 60px;
  }
  .row + .row,
  .stack + .stack {
    border-top: 1px solid var(--line);
  }
  .stack {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 14px 18px;
  }
  .row-title {
    font-weight: 500;
  }
  .row-desc {
    margin-top: 2px;
    color: var(--text-2);
    font-size: 12.5px;
    overflow-wrap: anywhere;
    user-select: text;
  }

  .btn {
    height: 32px;
    padding: 0 14px;
    border: 1px solid var(--line-strong);
    border-radius: var(--radius-sm);
    background: var(--surface-2);
    cursor: pointer;
    transition:
      background var(--fast) var(--ease),
      border-color var(--fast) var(--ease);
  }
  .btn:hover:not(:disabled) {
    background: var(--surface-3);
  }
  .btn:active:not(:disabled) {
    transform: translateY(1px);
  }
  .btn:disabled {
    color: var(--text-3);
    cursor: default;
  }
  .btn.primary {
    border-color: var(--accent);
    background: var(--accent);
    color: #fff;
    font-weight: 500;
  }
  .btn.primary:hover {
    background: var(--accent-strong);
  }
  .btn.ghost {
    border-color: transparent;
    background: transparent;
    color: var(--text-2);
  }
  .btn.ghost:hover {
    color: var(--text);
  }
  .inline {
    display: flex;
    gap: 6px;
    flex: none;
  }

  .seg {
    display: inline-flex;
    flex: none;
    padding: 2px;
    border: 1px solid var(--line-strong);
    border-radius: var(--radius-sm);
    background: var(--surface-2);
  }
  .seg button {
    min-width: 52px;
    height: 28px;
    padding: 0 10px;
    border: 0;
    border-radius: 4px;
    background: transparent;
    color: var(--text-2);
    cursor: pointer;
  }
  .seg button:hover {
    color: var(--text);
  }
  .seg button.current {
    background: var(--surface-3);
    color: var(--text);
    box-shadow: inset 0 0 0 1px var(--line-strong);
  }

  select {
    min-width: 220px;
    height: 32px;
    padding: 0 8px;
    border: 1px solid var(--line-strong);
    border-radius: var(--radius-sm);
    background: var(--surface-2);
  }

  .range {
    display: flex;
    align-items: center;
    gap: 12px;
    flex: none;
  }
  .range output {
    min-width: 72px;
    color: var(--text-2);
    font-variant-numeric: tabular-nums;
    text-align: right;
  }
  input[type="range"] {
    width: 220px;
    accent-color: var(--accent);
  }

  .switch {
    position: relative;
    flex: none;
    width: 40px;
    height: 22px;
    border: 1px solid var(--line-strong);
    border-radius: 11px;
    background: var(--surface-3);
    cursor: pointer;
    transition: background var(--fast) var(--ease);
  }
  .switch span {
    position: absolute;
    top: 3px;
    left: 3px;
    width: 14px;
    height: 14px;
    border-radius: 50%;
    background: var(--text-2);
    transition: transform var(--fast) var(--ease);
  }
  .switch.on {
    border-color: var(--accent);
    background: var(--accent);
  }
  .switch.on span {
    transform: translateX(18px);
    background: #fff;
  }

  .presets {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(190px, 1fr));
    gap: 10px;
    margin-bottom: 14px;
  }
  .preset {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 14px 16px;
    border: 1px solid var(--line);
    border-radius: var(--radius);
    background: var(--surface);
    text-align: left;
    cursor: pointer;
    transition: border-color var(--fast) var(--ease);
  }
  .preset:hover {
    border-color: var(--line-strong);
  }
  .preset.current {
    border-color: var(--accent);
    box-shadow: inset 0 0 0 1px var(--accent);
  }
  .preset-name {
    font-weight: 600;
  }
  .preset-desc {
    color: var(--text-2);
    font-size: 12.5px;
  }

  .banner {
    margin: 0 0 14px;
    padding: 10px 14px;
    border-radius: var(--radius-sm);
    font-size: 13px;
    user-select: text;
  }
  .banner.confirm {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    border: 1px solid var(--line-strong);
    background: var(--surface);
  }
  .banner.info {
    border: 1px solid var(--line);
    background: var(--surface-2);
    color: var(--text-2);
  }
  .banner.warn {
    border: 1px solid color-mix(in srgb, var(--warn) 40%, transparent);
    background: var(--warn-soft);
    color: var(--text);
  }
</style>
