<script lang="ts">
  // Audio settings as a routing board: every track the clip can hold is a card with an on/off
  // switch, and apps are moved between cards by dragging (or the chip menu). Apps with sound
  // right now sit in a tray; "Not recorded" parks apps that must never be captured.
  import { onMount } from "svelte";
  import { flip } from "svelte/animate";
  import { fade, scale, slide } from "svelte/transition";
  import { backOut, cubicOut } from "svelte/easing";
  import { api, type AudioApp, type InputDevice, type Settings, type Status } from "./api";
  import AppIcon from "./AppIcon.svelte";
  import Toggle from "./Toggle.svelte";
  import Select from "./Select.svelte";
  import { friendlyName, rememberNames } from "./icons";
  import { copyText, showMenu, type MenuItem } from "./menu-state.svelte";

  let { s, status = null, edit }: { s: Settings; status?: Status | null; edit: (f: (d: Settings) => void, delay?: number) => void } = $props();

  type ListKey = "game" | "voice" | "media" | "ignored";
  const LISTS: ListKey[] = ["game", "voice", "media", "ignored"];
  const MAX: Record<ListKey, number> = { game: 32, voice: 32, media: 32, ignored: 64 };

  const appTracks = [
    {
      id: "game",
      title: "Game",
      color: "#ff9f0a",
      icon: "M6 11h4M8 9v4M15 12h.01M18 10h.01M17.3 5H6.7a4 4 0 0 0-3.9 3.1l-1.2 5.4A3 3 0 0 0 4.5 17a3 3 0 0 0 2.4-1.2L8 14h8l1.1 1.8A3 3 0 0 0 19.5 17a3 3 0 0 0 2.9-3.5l-1.2-5.4A4 4 0 0 0 17.3 5z",
      desc: "The game itself. Left empty, it records the full-screen app in front when replay starts.",
    },
    {
      id: "voice",
      title: "Voice chat",
      color: "#5e5ce6",
      icon: "M12 3a3 3 0 0 0-3 3v6a3 3 0 0 0 6 0V6a3 3 0 0 0-3-3zM5 11a7 7 0 0 0 14 0M12 18v3",
      desc: "Discord, TeamSpeak and friends, so you can turn your friends down later.",
    },
    {
      id: "media",
      title: "Music and media",
      color: "#ff375f",
      icon: "M9 18V6l11-2v12M9 18a3 3 0 1 1-6 0 3 3 0 0 1 6 0zM20 16a3 3 0 1 1-6 0 3 3 0 0 1 6 0z",
      desc: "Players and browsers. Handy to mute copyrighted music when you upload.",
    },
  ] as const;

  const titles: Record<ListKey, string> = { game: "Game", voice: "Voice chat", media: "Music and media", ignored: "Not recorded" };
  const colors: Record<ListKey, string> = { game: "#ff9f0a", voice: "#5e5ce6", media: "#ff375f", ignored: "#8e8e93" };

  const trackOn = (id: "game" | "voice" | "media") => s.audio.tracks[id];
  const setTrack = (id: "game" | "voice" | "media", v: boolean) => edit((d) => (d.audio.tracks[id] = v));

  // What the clip will hold, in file order.
  const written = $derived(
    [
      s.audio.tracks.mix && "Mix",
      s.audio.mic && "Mic",
      s.audio.tracks.game && "Game",
      s.audio.tracks.voice && "Voice",
      s.audio.tracks.media && "Media",
    ].filter(Boolean) as string[],
  );

  // App tracks that are off: their apps are muted (captured nowhere, not even in the Mix).
  const mutedTracks = $derived(appTracks.filter((t) => !s.audio.tracks[t.id]).map((t) => t.title));

  // When the Game list is empty, show the currently captured window/fullscreen process.
  const autoGame = $derived(
    s.audio.game.length === 0 && status?.capture_target.kind === "window" && status.capture_target.exe
      ? status.capture_target.exe
      : null,
  );

  // ---- Microphones ----
  // The list follows the system: it reloads when the page opens, on a timer, when the window
  // regains focus and whenever the dropdown opens, so plugging in a headset just shows up.
  let inputs = $state<InputDevice[]>([]);
  let inputsLoaded = $state(false);
  async function loadInputs() {
    try {
      inputs = await api.audioInputs();
      inputsLoaded = true;
    } catch {
      /* keep the last list */
    }
  }
  const chosenMissing = $derived(
    inputsLoaded && s.audio.mic_device !== null && !inputs.some((d) => d.id === s.audio.mic_device),
  );
  const micOptions = $derived.by(() => {
    const def = inputs.find((d) => d.is_default);
    const list: { value: string | null; label: string; sub?: string; badge?: string; disabled?: boolean }[] = [
      { value: null, label: "System default", sub: def?.name ?? (inputsLoaded ? "No microphone found" : "Looking…") },
    ];
    for (const d of inputs) list.push({ value: d.id, label: d.name, badge: d.is_default ? "Default" : undefined });
    if (chosenMissing) {
      list.push({ value: s.audio.mic_device, label: "Disconnected microphone", sub: "Using the system default until it is back", disabled: true });
    }
    return list;
  });

  // ---- Running apps ----
  let running = $state<AudioApp[]>([]);
  let loading = $state(false);
  async function loadRunning() {
    loading = true;
    try {
      running = await api.audioApps();
      rememberNames(running);
    } catch {
      running = [];
    }
    loading = false;
  }
  onMount(() => {
    loadRunning();
    loadInputs();
    const t = setInterval(() => {
      if (document.hidden) return;
      loadRunning();
      loadInputs();
    }, 4000);
    return () => clearInterval(t);
  });

  const lower = (x: string) => x.toLowerCase();

  // ---- Which listed apps exist on this machine ----
  // Defaults name programs most people do not have (TeamSpeak, Opera...). Those stay in the
  // settings but are hidden here; `null` = not checked yet. Apps added or moved this session
  // always show, and "hidden" can be revealed.
  let present = $state<Set<string> | null>(null);
  const pinned = new Set<string>();
  let reveal = $state<Record<ListKey, boolean>>({ game: false, voice: false, media: false, ignored: false });
  const listed = $derived(LISTS.flatMap((k) => s.audio[k]).map(lower).sort().join("|"));
  $effect(() => {
    const all = listed ? listed.split("|") : [];
    let live = true;
    api
      .presentApps(all)
      .then((p) => live && (present = new Set(p)))
      .catch(() => live && (present = new Set(all)));
    return () => (live = false);
  });
  const shown = (k: ListKey, x: string) =>
    reveal[k] || pinned.has(lower(x)) || (present?.has(lower(x)) ?? false);
  const visible = (k: ListKey) => s.audio[k].filter((x) => shown(k, x));
  const hiddenCount = (k: ListKey) => (present ? s.audio[k].filter((x) => !shown(k, x)).length : 0);
  function where(exe: string): ListKey | null {
    return LISTS.find((k) => s.audio[k].some((x) => lower(x) === lower(exe))) ?? null;
  }

  // ---- Moving apps ----
  let notice = $state("");
  let noticeTimer: ReturnType<typeof setTimeout> | undefined;
  function say(msg: string) {
    notice = msg;
    clearTimeout(noticeTimer);
    noticeTimer = setTimeout(() => (notice = ""), 4000);
  }
  function assign(exe: string, target: ListKey | null) {
    if (target && s.audio[target].some((x) => lower(x) === lower(exe))) return;
    if (target && s.audio[target].length >= MAX[target]) {
      say(`${titles[target]} holds at most ${MAX[target]} apps.`);
      return;
    }
    notice = "";
    if (target) pinned.add(lower(exe));
    edit((d) => {
      for (const k of LISTS) d.audio[k] = d.audio[k].filter((x) => lower(x) !== lower(exe));
      if (target) d.audio[target].push(exe);
    });
  }

  /** "C:\\x\\Game.EXE", "game" -> "Game.EXE", "game.exe" */
  function normalize(text: string): string | null {
    let name = text.trim().replace(/^.*[\\/]/, "");
    if (!name) return null;
    if (!/\.exe$/i.test(name)) name += ".exe";
    return /^[^<>:"/\\|?*\x00-\x1f]{1,60}\.exe$/i.test(name) ? name : null;
  }

  // Add by name
  let addTo = $state<ListKey | null>(null);
  let addText = $state("");
  let addError = $state("");
  let addBox = $state<HTMLInputElement>();
  async function startAdd(k: ListKey) {
    addTo = k;
    addText = "";
    addError = "";
    await Promise.resolve();
    addBox?.focus();
  }
  function commitAdd() {
    if (!addTo) return;
    if (!addText.trim()) {
      addTo = null;
      return;
    }
    const name = normalize(addText);
    if (!name) {
      addError = "Use a program name like game.exe";
      return;
    }
    assign(name, addTo);
    addTo = null;
  }

  // ---- Drag and drop ----
  let dragging = $state<string | null>(null);
  let over = $state<ListKey | "tray" | null>(null);

  function dragStart(e: DragEvent, exe: string) {
    dragging = exe;
    e.dataTransfer?.setData("text/plain", exe);
    if (e.dataTransfer) e.dataTransfer.effectAllowed = "move";
  }
  const dragEnd = () => ((dragging = null), (over = null));
  const accepts = (e: DragEvent) => dragging !== null || e.dataTransfer?.types.includes("Files");

  function dragOver(e: DragEvent, zone: ListKey | "tray") {
    if (!accepts(e)) return;
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = dragging ? "move" : "copy";
    over = zone;
  }
  function dragLeave(e: DragEvent) {
    if (!(e.currentTarget as HTMLElement).contains(e.relatedTarget as Node | null)) over = null;
  }
  function drop(e: DragEvent, zone: ListKey | "tray") {
    e.preventDefault();
    const names: string[] = [];
    if (dragging) names.push(dragging);
    else for (const f of Array.from(e.dataTransfer?.files ?? [])) if (/\.exe$/i.test(f.name)) names.push(f.name);
    if (!dragging && !names.length && e.dataTransfer?.files.length) say("Only .exe programs can be dropped here.");
    for (const n of names) assign(n, zone === "tray" ? null : zone);
    dragEnd();
  }

  // ---- Context menus (right-click, or click an app's name) ----
  const DEFAULTS = {
    voice: ["discord.exe", "ts3client_win64.exe", "teamspeak.exe"],
    media: ["spotify.exe", "chrome.exe", "msedge.exe", "firefox.exe", "brave.exe", "opera.exe"],
  };

  /** Every app, anywhere: where to record it, or never; plus file actions. */
  function appMenu(e: MouseEvent | KeyboardEvent, exe: string, path?: string) {
    const at = where(exe);
    const items: MenuItem[] = [
      { header: "Record on" },
      ...(["game", "voice", "media"] as const).map((k) => ({
        label: titles[k],
        dot: colors[k],
        checked: at === k,
        run: () => assign(exe, k),
      })),
      { separator: true },
      { label: "Never record this app", dot: colors.ignored, checked: at === "ignored", run: () => assign(exe, "ignored") },
      { label: "Remove from all lists", danger: true, disabled: !at, run: () => assign(exe, null) },
      { separator: true },
      {
        label: "Show file location",
        run: () => api.revealApp(exe).catch(() => say(`Could not find where ${friendlyName(exe)} is installed.`)),
      },
      { label: "Copy program name", hint: exe, run: () => copyText(exe) },
      ...(path ? [{ label: "Copy path", run: () => copyText(path) }] : []),
    ];
    showMenu(e, items, { app: exe });
  }

  /** A track card: switch it, add apps, tidy its list. */
  function trackMenu(e: MouseEvent, k: ListKey) {
    const n = s.audio[k].length;
    const hidden = hiddenCount(k);
    const items: MenuItem[] = [{ header: titles[k] }];
    if (k !== "ignored") {
      const on = trackOn(k);
      items.push({ label: on ? "Mute this track" : "Unmute this track", run: () => setTrack(k, !on) });
    }
    items.push({ label: "Add app by name…", run: () => startAdd(k) });
    if (hidden > 0 || reveal[k]) {
      items.push({
        label: reveal[k] ? "Hide apps that are not installed" : `Show ${hidden} app${hidden === 1 ? "" : "s"} not installed`,
        run: () => (reveal[k] = !reveal[k]),
      });
    }
    items.push({ separator: true });
    if (k === "voice" || k === "media") {
      const missing = DEFAULTS[k].filter((x) => !s.audio[k].some((y) => lower(y) === x));
      items.push({
        label: "Add the usual apps",
        disabled: missing.length === 0,
        run: () => edit((d) => d.audio[k].push(...missing)),
      });
    }
    if (k !== "ignored") {
      items.push({
        label: "Never record these apps",
        dot: colors.ignored,
        disabled: n === 0,
        run: () => edit((d) => ((d.audio.ignored = [...new Set([...d.audio.ignored, ...d.audio[k]])]), (d.audio[k] = []))),
      });
    } else {
      items.push({
        label: "Allow all of these again",
        disabled: n === 0,
        run: () => edit((d) => (d.audio.ignored = [])),
      });
    }
    items.push({ label: `Clear ${n} app${n === 1 ? "" : "s"}`, danger: true, disabled: n === 0, run: () => edit((d) => (d.audio[k] = [])) });
    showMenu(e, items);
  }

  /** Mix and Microphone have no apps: just their switch. */
  function switchMenu(e: MouseEvent, title: string, on: boolean, set: (v: boolean) => void) {
    showMenu(e, [
      { header: title },
      { label: on ? "Turn track off" : "Turn track on", run: () => set(!on) },
      { separator: true },
      { label: "Reset audio to defaults", run: resetAudio },
    ]);
  }

  function trayMenu(e: MouseEvent) {
    showMenu(e, [
      { label: "Refresh list", run: loadRunning },
      { label: "Park every unassigned app under Not recorded", disabled: !running.some((a) => !where(a.exe)), run: parkUnassigned },
    ]);
  }
  function parkUnassigned() {
    const rest = running.filter((a) => !where(a.exe)).map((a) => a.exe);
    for (const x of rest) pinned.add(lower(x));
    edit((d) => d.audio.ignored.push(...rest));
  }

  function resetAudio() {
    edit((d) => {
      d.audio.enabled = true;
      d.audio.mic = true;
      d.audio.tracks = { mix: true, game: true, voice: true, media: true };
      d.audio.ignored = [];
      d.audio.game = [];
      d.audio.voice = [...DEFAULTS.voice];
      d.audio.media = [...DEFAULTS.media];
    });
  }
</script>

<svelte:window onfocus={loadInputs} />

{#snippet chip(exe: string, k: ListKey)}
  <span
      class="chip"
      class:lifted={dragging === exe}
      role="listitem"
      draggable="true"
      ondragstart={(e) => dragStart(e, exe)}
      ondragend={dragEnd}
      oncontextmenu={(e) => appMenu(e, exe)}
    >
      <AppIcon {exe} size={18} />
      <button class="chip-name" aria-haspopup="menu" title="{exe}. Click or right-click for options" onclick={(e) => appMenu(e, exe)}>
        {friendlyName(exe)}
      </button>
      <button class="chip-x" aria-label="Remove {exe}" onclick={() => assign(exe, null)}>×</button>
    </span>
{/snippet}

{#snippet dropzone(k: ListKey, hint: string)}
  <div
    class="zone"
    class:over={over === k}
    class:ready={dragging !== null && over !== k}
    role="list"
    aria-label="{titles[k]} apps"
    ondragover={(e) => dragOver(e, k)}
    ondragleave={dragLeave}
    ondrop={(e) => drop(e, k)}
  >
    {#each visible(k) as exe (lower(exe))}
      <span class="chip-wrap" animate:flip={{ duration: 320, easing: cubicOut }} in:scale={{ duration: 280, start: 0.7, easing: backOut }} out:fade={{ duration: 120 }}>
        {@render chip(exe, k)}
      </span>
    {:else}
      {#if k === "game" && autoGame}
        <span
          class="chip auto"
          role="listitem"
          title="{autoGame} — automatically captured"
          in:scale={{ duration: 280, start: 0.7, easing: backOut }}
          out:fade={{ duration: 120 }}
        >
          <AppIcon exe={autoGame} size={18} />
          <span class="chip-name">{friendlyName(autoGame)}</span>
          <span class="chip-tag">Auto</span>
        </span>
      {:else if present}
        <span class="empty" transition:fade={{ duration: 120 }}>{hint}</span>
      {/if}
    {/each}
    {#if addTo === k}
      <input
        class="add-box"
        bind:this={addBox}
        bind:value={addText}
        placeholder="game.exe"
        spellcheck="false"
        aria-label="Add a program to {titles[k]}"
        onkeydown={(e) => (e.key === "Enter" ? (e.preventDefault(), commitAdd()) : e.key === "Escape" && ((addTo = null), e.stopPropagation()))}
        onblur={commitAdd}
        transition:scale={{ duration: 200, start: 0.8, easing: backOut }}
      />
    {:else}
      <button class="add" onclick={() => startAdd(k)} aria-label="Add a program to {titles[k]} by name">+</button>
    {/if}
  </div>
  {#if hiddenCount(k) > 0 || reveal[k]}
    <button class="hidden-note" transition:slide={{ duration: 180 }} onclick={() => (reveal[k] = !reveal[k])}>
      {#if reveal[k]}Hide apps that are not installed{:else}{hiddenCount(k)} not installed · show{/if}
    </button>
  {/if}
  {#if addTo === k && addError}<p class="err" transition:slide={{ duration: 160 }}>{addError}</p>{/if}
{/snippet}

<section class="master">
  <div class="m-row">
    <div class="m-text">
      <div class="m-title">Record audio</div>
      <div class="m-desc">Each track below is saved separately in the clip, so you can mix them later in your editor.</div>
    </div>
    <Toggle on={s.audio.enabled} label="Record audio" onchange={(v) => edit((d) => (d.audio.enabled = v))} />
  </div>
</section>

<div class="board" class:off={!s.audio.enabled}>
  <p class="summary">
    {#if !s.audio.enabled}
      Audio is off. Clips are saved without sound.
    {:else if written.length}
      <span>Each clip holds <b>{written.length} audio track{written.length === 1 ? "" : "s"}</b>:</span>
      {#each written as t (t)}<span class="pill" transition:scale={{ duration: 220, start: 0.7, easing: backOut }}>{t}</span>{/each}
    {:else}
      <span class="warn">Every track is off, so clips will have no sound.</span>
    {/if}
    {#if s.audio.enabled && mutedTracks.length}
      <span class="muted-line" transition:slide={{ duration: 200 }}>Muted, silent in every track including the Mix: {mutedTracks.join(", ")}</span>
    {/if}
  </p>

  {#if s.audio.enabled && s.container === "mp4" && written.length > 1}
    <div class="fmt-note" transition:slide={{ duration: 220 }}>
      <span>
        Every track is named in the file ({written.join(", ")}), but MP4 stores names where most players do not look, so
        they show up as "aac 2ch…". MKV names them in mpv, VLC and most editors.
      </span>
      <button class="fmt-btn" onclick={() => edit((d) => (d.container = "mkv"))}>Use MKV</button>
    </div>
  {/if}

  <div class="a-main">
   <div class="a-tracks">
  {#if notice}<p class="notice" transition:slide={{ duration: 200 }}>{notice}</p>{/if}

  <div class="grid">
   <div class="col">
    <article class="track fixed" style="--c: var(--accent)" oncontextmenu={(e) => switchMenu(e, "Mix", s.audio.tracks.mix, (v) => edit((d) => (d.audio.tracks.mix = v)))}>
      <header>
        <span class="tile"><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M4 8v8M8 5v14M12 9v6M16 6v12M20 10v4" /></svg></span>
        <div class="t-title">Mix</div>
        <Toggle on={s.audio.tracks.mix} label="Mix track" onchange={(v) => edit((d) => (d.audio.tracks.mix = v))} />
      </header>
      <p class="t-desc">Everything you record, combined and level-limited. Plays by default.</p>
    </article>

    <article class="track fixed grow" style="--c: #30d158" oncontextmenu={(e) => switchMenu(e, "Microphone", s.audio.mic, (v) => edit((d) => (d.audio.mic = v)))}>
      <header>
        <span class="tile"><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 3a3 3 0 0 0-3 3v6a3 3 0 0 0 6 0V6a3 3 0 0 0-3-3zM5 11a7 7 0 0 0 14 0M12 18v3" /></svg></span>
        <div class="t-title">Microphone</div>
        <Toggle on={s.audio.mic} label="Microphone track" onchange={(v) => edit((d) => (d.audio.mic = v))} />
      </header>
      <p class="t-desc">Off silences it right away, mid-recording too. Pick the input to record from:</p>
      <Select
        label="Microphone"
        options={micOptions}
        value={s.audio.mic_device}
        disabled={!s.audio.mic}
        minWidth={0}
        popupMin={320}
        fill
        onopen={loadInputs}
        onselect={(v) => edit((d) => (d.audio.mic_device = v))}
      />
      {#if chosenMissing}
        <p class="t-note" transition:slide={{ duration: 200 }}>That microphone is not connected, so the system default is used. It switches back when it returns.</p>
      {/if}
    </article>
   </div>

    {#each appTracks as t (t.id)}
      {@const on = trackOn(t.id)}
      <article class="track" class:muted={!on} style="--c: {t.color}" oncontextmenu={(e) => trackMenu(e, t.id)}>
        <header>
          <span class="tile"><svg viewBox="0 0 24 24" aria-hidden="true"><path d={t.icon} /></svg></span>
          <div class="t-title">{t.title}</div>
          <span class="count">{visible(t.id).length + (t.id === "game" && autoGame ? 1 : 0)}</span>
          <Toggle {on} label="{t.title} track" onchange={(v) => setTrack(t.id, v)} />
        </header>
        <p class="t-desc">{t.desc}</p>
        {@render dropzone(t.id, t.id === "game" ? "Automatic: the full-screen app" : "Drop apps here")}
        {#if !on}
          <p class="t-note" transition:slide={{ duration: 200 }}>Muted. These apps are silent in the clip, including the Mix.</p>
        {/if}
      </article>
    {/each}

    <article class="track parked" style="--c: #8e8e93" oncontextmenu={(e) => trackMenu(e, "ignored")}>
      <header>
        <span class="tile"><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M5.6 5.6l12.8 12.8M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18z" /></svg></span>
        <div class="t-title">Not recorded</div>
        <span class="count">{visible("ignored").length}</span>
      </header>
      <p class="t-desc">Parked apps are never captured, on any track or in the Mix. Park them here instead of deleting, so they are one drag away.</p>
      {@render dropzone("ignored", "Drop apps you never want in a clip")}
    </article>
  </div>

  <div class="foot-row">
    <button class="btn ghost" onclick={resetAudio}>Reset audio to defaults</button>
  </div>
   </div>
   <aside class="a-apps">
  <div class="a-title">App routing<span>Playing now</span></div>
  <section
    class="tray"
    class:over={over === "tray"}
    aria-label="Apps with sound"
    ondragover={(e) => dragOver(e, "tray")}
    ondragleave={dragLeave}
    ondrop={(e) => drop(e, "tray")}
    oncontextmenu={trayMenu}
  >
    <div class="tray-list">
      {#each running as app (app.exe)}
        {@const at = where(app.exe)}
        <span
          class="app"
          class:lifted={dragging === app.exe}
          role="listitem"
          draggable="true"
          ondragstart={(e) => dragStart(e, app.exe)}
          ondragend={dragEnd}
          oncontextmenu={(e) => appMenu(e, app.exe, app.path)}
          animate:flip={{ duration: 320, easing: cubicOut }}
          in:scale={{ duration: 280, start: 0.7, easing: backOut }}
          out:fade={{ duration: 120 }}
        >
          <AppIcon exe={app.exe} size={22} />
          <span class="app-text">
            <span class="app-name">{friendlyName(app.exe)}</span>
            <span class="app-state">
              {#if app.playing}<span class="eq" aria-label="Playing"><i></i><i></i><i></i></span>{/if}
              {#if at}<i class="dot" style="background: {colors[at]}"></i>{titles[at]}{#if at !== "ignored" && !trackOn(at)}&nbsp;· muted{/if}{:else}Unassigned{/if}
            </span>
          </span>
        </span>
      {:else}
        <span class="empty">{loading ? "Looking for apps…" : "Nothing is making sound. Start a game, call or song and it shows up here."}</span>
      {/each}
    </div>
    <button class="icon-btn refresh" class:spin={loading} aria-label="Refresh" title="Refresh" onclick={loadRunning}>
      <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M3 12a9 9 0 0 1 15.5-6.2M21 4v5h-5M21 12a9 9 0 0 1-15.5 6.2M3 20v-5h5" /></svg>
    </button>
  </section>
  <p class="hint">Drag an app onto a track. You can also drop an .exe from Explorer, or add one by name with +. Drop an app back here to remove it.</p>

   </aside>
  </div>
</div>

<style>
  .master {
    border-radius: var(--radius);
    background: var(--surface);
    box-shadow: inset 0 0 0 1px var(--line);
  }
  .m-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 24px;
    padding: 14px 18px;
    min-height: 60px;
  }
  .m-title {
    font-size: 13.5px;
    font-weight: 550;
  }
  .m-desc {
    margin-top: 2px;
    color: var(--text-3);
    font-size: 12px;
    font-weight: 500;
  }
  .board {
    transition:
      opacity var(--med) var(--ease),
      filter var(--med) var(--ease);
  }
  .board.off {
    opacity: 0.4;
    filter: saturate(0.5);
    pointer-events: none;
  }

  .summary {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px;
    margin: 14px 4px 0;
    color: var(--text-2);
    font-size: 13px;
  }
  .summary b {
    color: var(--text);
  }
  .pill {
    padding: 2px 10px;
    border-radius: 999px;
    background: var(--accent-soft);
    color: var(--accent);
    font-size: 12px;
    font-weight: 650;
  }
  .warn {
    color: var(--warn);
  }
  .fmt-note {
    display: flex;
    align-items: center;
    gap: 14px;
    margin-top: 12px;
    padding: 10px 14px;
    border-radius: var(--radius-md);
    background: var(--accent-soft);
    color: var(--text-2);
    font-size: 12.5px;
    line-height: 1.4;
  }
  .fmt-btn {
    flex: none;
    height: 30px;
    padding: 0 14px;
    border: 0;
    border-radius: var(--radius-sm);
    background: var(--accent);
    color: var(--on-accent);
    font-size: 12.5px;
    font-weight: 650;
    cursor: pointer;
    transition:
      background var(--fast) var(--ease),
      transform var(--med) var(--spring);
  }
  .fmt-btn:hover {
    background: var(--accent-strong);
  }
  .fmt-btn:active {
    transform: scale(0.96);
  }
  .muted-line {
    flex-basis: 100%;
    color: var(--warn);
    font-size: 12px;
  }
  .hint {
    margin: 8px 4px 0;
    color: var(--text-3);
    font-size: 12px;
  }
  .notice {
    margin: 10px 0 0;
    padding: 9px 14px;
    border-radius: var(--radius-md);
    background: var(--warn-soft);
    font-size: 12.5px;
  }
  .err {
    margin: 6px 2px 0;
    color: var(--warn);
    font-size: 12px;
  }

  /* ---- Playing now ---- */
  .tray {
    position: relative;
    display: flex;
    gap: 8px;
    min-height: 66px;
    padding: 10px;
    border-radius: var(--radius);
    background: var(--surface);
    box-shadow: inset 0 0 0 1px var(--line);
    transition:
      box-shadow var(--med) var(--ease),
      background var(--med) var(--ease);
  }
  .tray.over {
    background: var(--surface-2);
    box-shadow: inset 0 0 0 1.5px var(--text-3);
  }
  .tray-list {
    display: flex;
    flex: 1;
    flex-wrap: wrap;
    gap: 8px;
  }
  .app {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 7px 12px 7px 9px;
    border-radius: var(--radius-md);
    background: var(--surface-2);
    cursor: grab;
    transition:
      background var(--fast) var(--ease),
      transform var(--med) var(--spring),
      opacity var(--fast) var(--ease);
  }
  .app:hover {
    background: var(--surface-3);
    transform: translateY(-1px);
  }
  .app:active {
    cursor: grabbing;
  }
  .lifted {
    opacity: 0.45;
    transform: scale(0.96);
  }
  .app-text {
    display: grid;
    line-height: 1.2;
  }
  .app-name {
    font-size: 12.5px;
    font-weight: 600;
  }
  .app-state {
    display: flex;
    align-items: center;
    gap: 5px;
    color: var(--text-3);
    font-size: 11px;
  }
  .dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
  }
  .eq {
    display: inline-flex;
    align-items: flex-end;
    gap: 1.5px;
    height: 10px;
    margin-right: 2px;
  }
  .eq i {
    width: 2px;
    height: 100%;
    border-radius: 1px;
    background: var(--ok);
    transform-origin: bottom;
    animation: eq 0.9s ease-in-out infinite;
  }
  .eq i:nth-child(2) {
    animation-delay: 0.15s;
  }
  .eq i:nth-child(3) {
    animation-delay: 0.3s;
  }
  @keyframes eq {
    0%,
    100% {
      transform: scaleY(0.3);
    }
    50% {
      transform: scaleY(1);
    }
  }
  .icon-btn {
    display: grid;
    flex: none;
    align-self: flex-start;
    place-items: center;
    width: 28px;
    height: 28px;
    padding: 0;
    border: 0;
    border-radius: 8px;
    background: transparent;
    color: var(--text-3);
    cursor: pointer;
    transition:
      background var(--fast) var(--ease),
      color var(--fast) var(--ease);
  }
  .icon-btn:hover {
    background: var(--surface-3);
    color: var(--text);
  }
  .icon-btn svg,
  .tile svg {
    fill: none;
    stroke: currentColor;
    stroke-width: 2;
    stroke-linecap: round;
    stroke-linejoin: round;
  }
  .icon-btn svg {
    width: 15px;
    height: 15px;
  }
  .refresh.spin svg {
    animation: spin 0.8s linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }

  /* ---- Tracks ---- */
  .grid {
    display: grid;
    grid-template-columns: repeat(5, minmax(0, 1fr));
    align-items: stretch;
    gap: 12px;
  }
  .col {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }
  .track.grow {
    flex: 1;
  }
  .track {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 14px;
    border-radius: var(--radius);
    background: var(--surface);
    box-shadow: inset 0 0 0 1px var(--line);
    transition:
      opacity var(--med) var(--ease),
      background var(--med) var(--ease);
  }
  .track.fixed {
    min-height: 0;
  }
  .track.parked {
    background: transparent;
    box-shadow: inset 0 0 0 1.5px var(--line-strong);
    background-image: repeating-linear-gradient(135deg, transparent 0 10px, var(--surface) 10px 11px);
  }
  header {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px;
  }
  .tile {
    order: -2;
    margin-right: auto;
    display: grid;
    place-items: center;
    width: 28px;
    height: 28px;
    border-radius: 9px;
    background: var(--c);
    color: #fff;
  }
  .tile svg {
    width: 16px;
    height: 16px;
  }
  .t-title {
    order: 3;
    flex-basis: 100%;
    margin-top: 2px;
    font: 650 14px var(--font-display);
  }
  .count {
    order: -1;
    min-width: 22px;
    padding: 1px 7px;
    border-radius: 999px;
    background: var(--surface-3);
    color: var(--text-2);
    font-size: 11.5px;
    font-weight: 650;
    text-align: center;
    font-variant-numeric: tabular-nums;
  }
  .t-desc {
    margin: 0;
    color: var(--text-3);
    font-size: 12px;
    line-height: 1.4;
  }
  .t-note {
    margin: 0;
    color: var(--text-2);
    font-size: 12px;
  }
  .track.muted .zone {
    opacity: 0.55;
  }

  .zone {
    display: flex;
    flex: 1;
    flex-direction: column;
    align-items: stretch;
    gap: 6px;
    min-height: 220px;
    padding: 7px;
    border-radius: var(--radius-md);
    background: var(--surface-2);
    box-shadow: inset 0 0 0 1.5px transparent;
    transition:
      box-shadow var(--med) var(--ease),
      background var(--med) var(--ease),
      transform var(--med) var(--spring);
  }
  .zone.ready {
    box-shadow: inset 0 0 0 1.5px var(--line-strong);
  }
  .zone.over {
    background: color-mix(in srgb, var(--c) 18%, transparent);
    box-shadow: inset 0 0 0 1.5px var(--c);
    transform: scale(1.01);
  }
  .hidden-note {
    align-self: flex-start;
    margin: 0 2px;
    padding: 2px 6px;
    border: 0;
    border-radius: 6px;
    background: transparent;
    color: var(--text-3);
    font-size: 11.5px;
    cursor: pointer;
    transition:
      background var(--fast) var(--ease),
      color var(--fast) var(--ease);
  }
  .hidden-note:hover {
    background: var(--surface-2);
    color: var(--text-2);
  }
  .empty {
    margin: auto 0;
    text-align: center;
    padding: 0 6px;
    color: var(--text-3);
    font-size: 12px;
  }

  .chip-wrap {
    position: relative;
  }
  .chip-wrap .chip {
    display: flex;
  }
  .chip {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 3px 3px 3px 7px;
    border-radius: 9px;
    background: var(--surface-3);
    cursor: grab;
    transition:
      background var(--fast) var(--ease),
      transform var(--med) var(--spring),
      opacity var(--fast) var(--ease);
  }
  .chip:hover {
    background: var(--line-strong);
  }
  .chip.auto {
    cursor: default;
    padding: 3px 7px;
    background: color-mix(in srgb, var(--c) 14%, var(--surface-3));
    box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--c) 35%, transparent);
  }
  .chip.auto:hover {
    background: color-mix(in srgb, var(--c) 18%, var(--surface-3));
  }
  .chip-tag {
    flex: none;
    padding: 1px 6px;
    border-radius: 999px;
    background: color-mix(in srgb, var(--c) 22%, transparent);
    color: var(--c);
    font-size: 10.5px;
    font-weight: 650;
  }
  .chip-name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-align: left;
    text-overflow: ellipsis;
    white-space: nowrap;
    padding: 0;
    border: 0;
    background: transparent;
    font-size: 12.5px;
    font-weight: 600;
    cursor: inherit;
  }
  .chip-x {
    width: 20px;
    height: 20px;
    padding: 0;
    border: 0;
    border-radius: 6px;
    background: transparent;
    color: var(--text-3);
    cursor: pointer;
    transition:
      background var(--fast) var(--ease),
      color var(--fast) var(--ease);
  }
  .chip-x:hover {
    background: var(--rec-soft);
    color: var(--rec);
  }
  .add {
    align-self: flex-start;
    width: 26px;
    height: 26px;
    padding: 0;
    border: 0;
    border-radius: 8px;
    background: transparent;
    color: var(--text-3);
    font-size: 16px;
    cursor: pointer;
    transition:
      background var(--fast) var(--ease),
      color var(--fast) var(--ease),
      transform var(--med) var(--spring);
  }
  .add:hover {
    background: var(--surface-3);
    color: var(--text);
  }
  .add:active {
    transform: scale(0.9);
  }
  .add-box {
    width: 100%;
    height: 26px;
    padding: 0 9px;
    border: 0;
    border-radius: 8px;
    outline: 0;
    background: var(--accent-soft);
    box-shadow: inset 0 0 0 1.5px var(--accent);
    font-size: 12.5px;
    user-select: text;
  }

  .foot-row {
    display: flex;
    justify-content: flex-end;
    margin-top: 14px;
  }
  .btn {
    height: 32px;
    padding: 0 15px;
    border: 0;
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--text-2);
    font-size: 12.5px;
    font-weight: 600;
    cursor: pointer;
    transition:
      background var(--fast) var(--ease),
      color var(--fast) var(--ease),
      transform var(--med) var(--spring);
  }
  .btn:hover {
    background: var(--surface-2);
    color: var(--text);
  }
  .btn:active {
    transform: scale(0.96);
  }

  /* ---- Layout: tracks left, app routing right ---- */
  .a-main {
    display: grid;
    grid-template-columns: minmax(0, 1fr) 300px;
    gap: 16px;
    margin-top: 14px;
    align-items: start;
  }
  .a-tracks {
    min-width: 0;
  }
  .a-apps {
    position: sticky;
    top: 0;
    min-width: 0;
    padding: 14px;
    border-radius: 18px;
    background: var(--surface-2);
  }
  .a-title {
    display: flex;
    align-items: baseline;
    gap: 8px;
    margin: 0 0 10px 2px;
    font: 650 14px var(--font-display);
  }
  .a-title span {
    color: var(--text-3);
    font: 500 12px var(--font);
  }
  .a-apps .tray {
    flex-direction: column;
    min-height: 200px;
    padding: 8px;
    background: var(--surface);
  }
  .a-apps .tray-list {
    flex-direction: column;
    flex-wrap: nowrap;
  }
  .a-apps .app {
    width: 100%;
  }
  .a-apps .refresh {
    align-self: flex-end;
  }
  .a-apps .hint {
    margin: 10px 2px 0;
  }
  .master {
    margin-bottom: 0;
  }
</style>
