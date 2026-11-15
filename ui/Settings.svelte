<script lang="ts">
  import { onMount, tick } from "svelte";
  import { slide, scale, fade, fly } from "svelte/transition";
  import { cubicOut, backOut } from "svelte/easing";
  import { api, errorText, onStatus, onUpdate, onBadgeIcon, type AppInfo, type BadgeCorner, type CodecSupport, type Display, type PresetName, type Settings, type Status, type UpdateInfo } from "./api";
  import HotkeyInput from "./HotkeyInput.svelte";
  import AudioPage from "./AudioPage.svelte";
  import ClipsPage from "./ClipsPage.svelte";
  import BadgeIcon, { BADGE_ICONS } from "./BadgeIcon.svelte";
  import Toggle from "./Toggle.svelte";
  import Seg from "./Seg.svelte";
  import SliderField from "./SliderField.svelte";
  import Select from "./Select.svelte";
  import Rail from "./Rail.svelte";
  import TitleBar from "./TitleBar.svelte";
  import AppIcon from "./AppIcon.svelte";
  import { friendlyName, rememberNames } from "./icons";
  import type { SourceSettings, WindowInfo } from "./api";
  import { dragscroll } from "./dragscroll";
  import ContextMenu from "./ContextMenu.svelte";
  import changelogRaw from "../CHANGELOG.md?raw";
  import { parseChangelog } from "./markdown";
  import { copyText, showMenu, type MenuItem } from "./menu-state.svelte";

  let { initial }: { initial: Settings } = $props();

  // The page owns its draft after the first load.
  // svelte-ignore state_referenced_locally
  let s = $state<Settings>(structuredClone(initial));
  let status = $state<Status | null>(null);
  let displays = $state<Display[]>([]);
  let defaultDir = $state("");
  let estimate = $state(0);
  const fmtSize = (mb: number) => (mb >= 1000 ? `${(mb / 1000).toFixed(1)} GB` : `${mb} MB`);
  let saveError = $state("");
  let actionError = $state("");
  let settingsOpen = $state(false);
  let dashEl = $state<HTMLElement>();
  let tab = $state<"general" | "appearance" | "updates" | "about">("general");
  const tabs = [
    { id: "general", label: "General", tile: "#8e8e93", icon: "M4 7h9M17 7h3M4 17h3M11 17h9M15 4v6M9 14v6" },
    { id: "appearance", label: "Appearance", tile: "#bf5af2", icon: "M12 3a9 9 0 1 0 0 18c1.4 0 2-1 1.5-2-.6-1.3.2-2.5 1.6-2.5H17a4 4 0 0 0 4-4C21 6.6 17 3 12 3zM7.5 11h.01M10 7.5h.01M14.5 7.5h.01" },
    { id: "updates", label: "Updates", tile: "#30d158", icon: "M12 4v11M7 11l5 5 5-5M5 20h14" },
    { id: "about", label: "About", tile: "#0a84ff", icon: "M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18zM12 11v5M12 8h.01" },
  ] as const;


  const presets: { id: PresetName; name: string; desc: string }[] = [
    { id: "compat", name: "Compatible", desc: "H.264 1080p60. Plays on any device and site." },
    { id: "balanced", name: "Balanced", desc: "Best codec your GPU has, 1080p60." },
    { id: "high", name: "High", desc: "Your monitor's own resolution, 60 fps, higher quality." },
    { id: "custom", name: "Custom", desc: "Pick resolution, frame rate, codec and quality." },
  ];

  const resolutions = [
    { w: 0, h: 0, label: "Monitor size" },
    { w: 1280, h: 720, label: "1280 × 720", tag: "720p" },
    { w: 1920, h: 1080, label: "1920 × 1080", tag: "1080p" },
    { w: 2560, h: 1440, label: "2560 × 1440", tag: "1440p" },
    { w: 3840, h: 2160, label: "3840 × 2160", tag: "4K" },
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
    const { preset, custom, buffer_secs, container, clips_dir, cursor, display, source } = x;
    // The on/off switches of tracks the running session has mute live: they never restart capture.
    const audio = { ...x.audio, tracks: { ...x.audio.tracks } };
    const live = status?.audio_live;
    if (live) {
      audio.tracks.mix ||= live.mix;
      audio.mic ||= live.mic;
      if (live.mic) audio.mic_device = null; // switching microphones is live too
      audio.tracks.game ||= live.game;
      audio.tracks.voice ||= live.voice;
      audio.tracks.media ||= live.media;
    }
    // Moving, parking or releasing apps re-routes the running mixer live (no restart).
    audio.game = [];
    audio.voice = [];
    audio.media = [];
    audio.ignored = [];
    return JSON.stringify({ preset, custom, buffer_secs, container, clips_dir, cursor, display, source, audio });
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

  /** Save a clip, then pop a check beside the buttons (Site Builder "success check"). */
  let savedFlash = $state(0);
  async function save(secs: number | null) {
    await act(() => api.saveClip(secs));
    if (!actionError) {
      savedFlash++;
      const n = savedFlash;
      setTimeout(() => savedFlash === n && (savedFlash = 0), 1800);
    }
  }

  const fmtBuffer = (v: number) => (v >= 60 ? `${Math.floor(v / 60)} min${v % 60 ? ` ${v % 60} s` : ""}` : `${v} s`);
  /** "90", "90s", "1:30" or "1m30s" to seconds. */
  function parseBuffer(text: string): number | null {
    const x = text.trim().toLowerCase();
    let m = x.match(/^(\d+):(\d{1,2})$/) ?? x.match(/^(\d+)\s*m(?:in)?\s*(\d+)?\s*s?$/);
    if (m) return +m[1] * 60 + +(m[2] ?? 0);
    m = x.match(/^(\d+(?:\.\d+)?)\s*s?$/);
    return m ? +m[1] : null;
  }

  const monitorOptions = $derived([
    { value: 0, label: "Main monitor" },
    ...(s.display && !displays.some((d) => d.number === s.display)
      ? [{ value: s.display, label: `Display ${s.display} (not connected)` }]
      : []),
    ...displays.map((d) => ({ value: d.number, label: d.label, badge: d.primary ? "Main" : undefined })),
  ]);

  // Shortcuts: defaults mirror HotkeySettings::default in suicap-core/config.rs.
  type HotkeyKey = keyof Settings["hotkeys"];
  const hotkeyDefs: { key: HotkeyKey; label: string; def: string }[] = [
    { key: "save_15", label: "Save last 15 seconds", def: "Ctrl+Alt+F9" },
    { key: "save_30", label: "Save last 30 seconds", def: "Ctrl+Alt+F10" },
    { key: "save_60", label: "Save last 60 seconds", def: "Ctrl+Alt+F11" },
    { key: "save_full", label: "Save whole buffer", def: "Ctrl+Alt+F12" },
    { key: "screenshot", label: "Take a screenshot", def: "Ctrl+Alt+F8" },
    { key: "target_app", label: "Record the app in front", def: "Ctrl+Alt+F7" },
  ];
  const hotkeyRows = $derived(
    hotkeyDefs.map((h) => {
      const value = s.hotkeys[h.key];
      const other = value
        ? hotkeyDefs.find((o) => o.key !== h.key && s.hotkeys[o.key].toLowerCase() === value.toLowerCase())
        : undefined;
      return { ...h, value, changed: value !== h.def, clash: other?.label ?? "" };
    }),
  );
  const resetHotkey = (key: HotkeyKey) => edit((d) => (d.hotkeys[key] = hotkeyDefs.find((h) => h.key === key)!.def));
  const resetAllHotkeys = () => edit((d) => hotkeyDefs.forEach((h) => (d.hotkeys[h.key] = h.def)));
  const clearAllHotkeys = () => edit((d) => hotkeyDefs.forEach((h) => (d.hotkeys[h.key] = "")));

  // ---- Context menus ----
  /** Replay status card and the sidebar status: turn on/off, save, open. */
  function replayMenu(e: MouseEvent) {
    const items: MenuItem[] = [
      running
        ? { label: "Turn replay off", run: () => act(api.disarm) }
        : { label: "Turn replay on", run: () => act(api.arm) },
      { separator: true },
      { header: "Save clip" },
      { label: "Last 15 seconds", disabled: !armed, run: () => save(15) },
      { label: "Last 30 seconds", disabled: !armed, run: () => save(30) },
      { label: "Last 60 seconds", disabled: !armed, run: () => save(60) },
      { label: "Whole buffer", disabled: !armed, run: () => save(null) },
      { separator: true },
      { label: "Show last clip", disabled: !status?.last_clip, run: () => act(api.revealLastClip) },
      { label: "Open clips folder", run: () => act(api.openClipsFolder) },
    ];
    showMenu(e, items);
  }

  function folderMenu(e: MouseEvent) {
    showMenu(e, [
      { label: "Open clips folder", run: () => act(api.openClipsFolder) },
      { label: "Change folder…", run: pickDir },
      { label: "Use the default folder", disabled: !s.clips_dir, run: () => edit((d) => (d.clips_dir = null)) },
      { separator: true },
      { label: "Copy path", run: () => copyText(s.clips_dir ?? defaultDir) },
    ]);
  }

  const hotkeyInputs: Record<string, { start: () => void } | undefined> = {};
  function hotkeyMenu(e: MouseEvent, r: (typeof hotkeyRows)[number]) {
    showMenu(e, [
      { label: "Change shortcut…", run: () => hotkeyInputs[r.key]?.start() },
      { label: "Reset to default", hint: r.def, disabled: !r.changed, run: () => resetHotkey(r.key) },
      { label: "Clear shortcut", danger: true, disabled: !r.value, run: () => edit((d) => (d.hotkeys[r.key] = "")) },
      { separator: true },
      { label: "Copy shortcut", disabled: !r.value, run: () => copyText(r.value) },
    ]);
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

  // ---- Updates ----
  let update = $state<UpdateInfo | null>(null);
  let appInfo = $state<AppInfo | null>(null);
  const upState = $derived(update?.state);
  const upBusy = $derived(
    upState?.phase === "checking" || upState?.phase === "downloading" || upState?.phase === "installing",
  );
  const upBadge = $derived(upState?.phase === "available" || upState?.phase === "ready");
  const lastChecked = $derived(
    update?.last_checked
      ? new Date(update.last_checked * 1000).toLocaleString([], { dateStyle: "medium", timeStyle: "short" })
      : "Not checked yet",
  );
  const upHead = $derived.by(() => {
    const st = upState;
    switch (st?.phase) {
      case "checking":
        return { title: "Checking for updates…", desc: `Version ${update?.current}` };
      case "up_to_date":
        return { title: "You're up to date", desc: `Version ${update?.current} is the latest` };
      case "available":
        return { title: `Version ${st.version} is available`, desc: `You have ${update?.current}` };
      case "downloading":
        return { title: `Downloading ${st.version}…`, desc: "Getting the update and checking it. You can keep using SuiCap." };
      case "ready":
        return { title: `Version ${st.version} is ready`, desc: "Download verified. SuiCap restarts to finish; replay resumes by itself." };
      case "installing":
        return { title: `Installing ${st.version}…`, desc: "SuiCap closes briefly and reopens after the update." };
      case "failed":
        return { title: st.version ? `Couldn't update to ${st.version}` : "Couldn't check for updates", desc: st.message };
      default:
        return { title: "Updates", desc: `Version ${update?.current ?? ""}` };
    }
  });
  const changelog = parseChangelog(changelogRaw);
  let showAllLogs = $state(false);
  const logs = $derived(showAllLogs ? changelog : changelog.slice(0, 3));
  /** What the badge shows right now, mirrored in the corner picker; a sample when nothing runs. */
  const badgePreview = $derived.by(() => {
    const clock = (t: number) => `${Math.floor(t / 60)}:${String(Math.floor(t % 60)).padStart(2, "0")}`;
    if (status?.recording) return { rec: true, sample: false, label: s.badge_label_rec, time: clock(status.recording_secs) };
    if (armed && s.badge_during_replay)
      return { rec: false, sample: false, label: s.badge_label_replay, time: `${Math.round(status?.buffered_secs ?? 0)} s` };
    return { rec: true, sample: true, label: s.badge_label_rec, time: "0:12" };
  });
  const badgeColors = [
    { key: "badge_bg", label: "Background", short: "Back" },
    { key: "badge_text", label: "Text", short: "Text" },
    { key: "badge_rec_color", label: "Recording dot", short: "Rec" },
    { key: "badge_replay_color", label: "Replay dot", short: "Replay" },
  ] as const;
  /** Factory look (mirrors Settings::default in suicap-core/config.rs). */
  const badgeDefaults = {
    badge_scale: 100,
    badge_opacity: 88,
    badge_margin: 16,
    badge_show_time: true,
    badge_clip_flash: true,
    badge_show_label: true,
    badge_label_rec: "REC",
    badge_label_replay: "REPLAY",
    badge_icon: "dot",
    badge_bg: "#121317",
    badge_text: "#f2f2f4",
    badge_rec_color: "#ef4444",
    badge_replay_color: "#3ba7ff",
  } as const;
  /** The user's own badge image (data URL), or null. */
  let customIcon = $state<string | null>(null);
  const loadCustomIcon = () => api.badgeIconData().then((d) => (customIcon = d)).catch(() => (customIcon = null));
  async function chooseIcon() {
    try {
      if (await api.pickBadgeIcon()) edit((d) => (d.badge_icon = "custom"));
      actionError = "";
    } catch (e) {
      actionError = errorText(e);
    }
  }
  async function removeIcon() {
    await act(api.removeBadgeIcon);
    if (s.badge_icon === "custom") edit((d) => (d.badge_icon = "dot"));
  }
  const resetBadgeStyle = () => edit((d) => Object.assign(d, badgeDefaults));
  /** The picker's preview badge uses the same colors and proportions as the real one. */
  const miniStyle = $derived(
    `--ms:${Math.min(s.badge_scale / 100, 1.6)};--mb:${s.badge_bg};--mf:${s.badge_text};--mrec:${s.badge_rec_color};--mrep:${s.badge_replay_color};--ma:${s.badge_opacity}%`,
  );
  /** What the GPU can encode; null until probed. Codecs it cannot do are greyed out. */
  let support = $state<CodecSupport | null>(null);
  const loadSupport = () => api.codecSupport().then((c) => (support = c)).catch(() => (support = null));
  const codecName = { av1: "AV1", hevc: "HEVC", h264: "H.264" } as const;
  const codecOptions = $derived(
    (
      [
        { value: null, label: "Auto" },
        { value: "av1", label: "AV1" },
        { value: "hevc", label: "HEVC" },
        { value: "h264", label: "H.264" },
      ] as const
    ).map((o) => {
      const missing = support && o.value !== null && !support.codecs.includes(o.value);
      return { ...o, disabled: Boolean(missing), hint: missing ? `${support?.gpu ?? "This GPU"} can't encode ${o.label}` : undefined };
    }),
  );
  const codecUnavailable = $derived(
    support && s.custom.codec && !support.codecs.includes(s.custom.codec) ? codecName[s.custom.codec] : "",
  );
  const corners: { id: BadgeCorner; label: string }[] = [
    { id: "top-left", label: "Top left" },
    { id: "top-right", label: "Top right" },
    { id: "bottom-left", label: "Bottom left" },
    { id: "bottom-right", label: "Bottom right" },
  ];
  const updateModes = [
    { value: "off", label: "Off" },
    { value: "notify", label: "Notify me" },
    { value: "auto", label: "Automatic" },
  ] as const;
  const updateModeDesc = $derived(
    {
      off: "SuiCap never checks by itself.",
      notify: "Checks every few hours; you choose when to install.",
      auto: "Downloads in the background and installs only while you're away from the PC.",
    }[s.update_mode],
  );

  // ---- Video source: display, one application, or the full-screen game ----
  const DEFAULT_SOURCE: SourceSettings = { kind: "display", exe: "", title: "", fallback: true };
  const src = $derived(s.source ?? DEFAULT_SOURCE);
  const setSource = (f: (x: SourceSettings) => void) =>
    edit((d) => {
      d.source = { ...DEFAULT_SOURCE, ...(d.source ?? {}) };
      f(d.source);
    });

  let windows = $state<WindowInfo[]>([]);
  async function loadWindows() {
    try {
      windows = await api.captureWindows();
    } catch {
      /* keep the last list */
    }
  }
  const KEY = "\u0000";
  const windowOptions = $derived.by(() => {
    const list: { value: string; label: string; sub?: string; exe?: string; disabled?: boolean; badge?: string }[] = windows.map((w) => ({
      value: w.exe + KEY + w.title,
      label: friendlyName(w.exe),
      sub: w.title,
      exe: w.exe,
    }));
    if (src.exe && !windows.some((w) => w.exe.toLowerCase() === src.exe.toLowerCase())) {
      list.unshift({ value: src.exe + KEY + src.title, label: friendlyName(src.exe), sub: "Not running", exe: src.exe, badge: "Waiting" });
    }
    return list;
  });
  const windowValue = $derived.by(() => {
    if (!src.exe) return "";
    const same = windowOptions.filter((o) => o.exe?.toLowerCase() === src.exe.toLowerCase());
    const exact = src.title ? same.find((o) => o.sub?.toLowerCase().includes(src.title.toLowerCase())) : undefined;
    return (exact ?? same[0])?.value ?? "";
  });
  function pickWindow(v: string) {
    const [exe, title] = v.split(KEY);
    setSource((x) => {
      x.exe = exe;
      x.title = x.title ? title : "";
    });
  }
  const exactOn = $derived(src.title !== "");
  function setExact(on: boolean) {
    const w = windows.find((w) => w.exe.toLowerCase() === src.exe.toLowerCase());
    setSource((x) => (x.title = on ? (w?.title ?? "") : ""));
  }

  let nameText = $state("");
  let nameError = $state("");
  function addByName() {
    let n = nameText.trim().split(/[\\/]/).pop() ?? "";
    if (!n) return;
    if (!/\.exe$/i.test(n)) n += ".exe";
    if (!/^[^<>:"/\\|?*\x00-\x1f]{1,60}\.exe$/i.test(n)) {
      nameError = "Use a program name like game.exe";
      return;
    }
    nameError = "";
    nameText = "";
    setSource((x) => ((x.kind = "app"), (x.exe = n.toLowerCase()), (x.title = "")));
  }

  const target = $derived(status?.capture_target);
  const sourceLine = $derived.by(() => {
    if (src.kind === "auto") {
      return target?.kind === "window" && target.exe
        ? `Following ${friendlyName(target.exe)}`
        : "No game right now: recording the display";
    }
    if (src.kind === "display") return "Recording the display";
    if (!src.exe) return "Choose an application to record";
    const name = friendlyName(src.exe);
    if (target?.kind === "window") return `Recording ${name} (window)`;
    if (target?.kind === "waiting") return `Waiting for ${name}`;
    return src.fallback ? `Waiting for ${name}: recording the display` : `Waiting for ${name}`;
  });
  const capturing = $derived(src.kind === "app" && target?.kind === "window");

  function gameMenu(e: MouseEvent) {
    if (!game) return showMenu(e, sourceItems());
    showMenu(e, sourceItems(), { app: game });
  }
  function sourceItems(): MenuItem[] {
    const g = game;
    return [
      { label: g ? `Record only ${friendlyName(g)}` : "Record only this app", disabled: !g, checked: src.kind === "app" && !!g && src.exe.toLowerCase() === g.toLowerCase(), run: () => g && recordOnly(g) },
      { label: "Record the display", checked: src.kind === "display", run: () => setSource((x) => (x.kind = "display")) },
      { label: "Auto-follow the game", checked: src.kind === "auto", run: () => setSource((x) => (x.kind = "auto")) },
    ];
  }
  const recordOnly = (exe: string) => setSource((x) => ((x.kind = "app"), (x.exe = exe), (x.title = "")));

  // The game in front: the full-screen app, else a running app from the Game track.
  let game = $state<string | null>(null);
  async function pollGame() {
    if (document.hidden) return;
    try {
      let g = await api.foregroundGame();
      if (!g && s.audio.tracks.game && s.audio.game.length) {
        const apps = await api.audioApps();
        rememberNames(apps);
        const mine = s.audio.game.map((x) => x.toLowerCase());
        g = apps.find((a) => a.playing && mine.includes(a.exe.toLowerCase()))?.exe ?? null;
      }
      game = g;
    } catch {
      /* keep the last value */
    }
  }

  onMount(() => {
    pollGame();
    if (src.kind === "app") loadWindows();
    const gameTimer = setInterval(pollGame, 3000);
    const winTimer = setInterval(() => !document.hidden && src.kind === "app" && loadWindows(), 4000);
    api.updateInfo().then((u) => (update = u));
    loadSupport();
    api.appInfo().then((i) => (appInfo = i));
    const unUpdate = onUpdate((u) => (update = u));
    loadCustomIcon();
    const unIcon = onBadgeIcon(loadCustomIcon);
    const un = onStatus((st) => (status = st));
    api.status().then((st) => (status = st));
    api.displays().then((d) => (displays = d));
    api.defaultClipsDir().then((d) => (defaultDir = d));
    api.estimateMb($state.snapshot(s) as Settings).then((m) => (estimate = m));
    return () => {
      clearInterval(gameTimer);
      clearInterval(winTimer);
      un.then((f) => f());
      unUpdate.then((f) => f());
      unIcon.then((f) => f());
    };
  });
</script>

{#snippet row(title: string, desc: string)}
  <div class="row-text">
    <div class="row-title">{title}</div>
    {#if desc}<div class="row-desc">{desc}</div>{/if}
  </div>
{/snippet}

<svelte:window onkeydown={(e) => e.key === "Escape" && !e.defaultPrevented && settingsOpen && (settingsOpen = false)} />

<div class="shell">
  <Rail
    scrollEl={dashEl}
    {armed}
    hasError={status?.phase === "error"}
    {settingsOpen}
    {upBadge}
    onOpenSettings={() => (settingsOpen = true)}
  />

  <div class="main-col">
    <header class="top" data-tauri-drag-region>
      <div class="word" data-tauri-drag-region>SuiCap</div>
      <!-- svelte-ignore a11y_no_static_element_interactions -->
      <div class="stats" data-tauri-drag-region oncontextmenu={replayMenu}>
        {#if game}
          <!-- svelte-ignore a11y_no_static_element_interactions -->
          <span class="stat game" class:capturing={capturing && src.exe.toLowerCase() === game.toLowerCase()} title={game} oncontextmenu={gameMenu} transition:scale={{ duration: 240, start: 0.8, easing: backOut }}>
            <AppIcon exe={game} size={18} />{friendlyName(game)}
            {#if capturing && src.exe.toLowerCase() === game.toLowerCase()}
              <em>Capturing</em>
              <button class="pill-x" aria-label="Record the display instead" title="Record the display instead" onclick={() => setSource((x) => (x.kind = "display"))}>
                <svg viewBox="0 0 12 12" aria-hidden="true"><path d="M3 3l6 6M9 3l-6 6" /></svg>
              </button>
            {:else}
              <button class="pill-act" class:always={src.kind === "display"} onclick={() => recordOnly(game!)}>Record only this</button>
            {/if}
          </span>
        {:else if capturing && src.exe}
          <span class="stat game capturing" title={src.exe} oncontextmenu={gameMenu}>
            <AppIcon exe={src.exe} size={18} />{friendlyName(src.exe)}<em>Capturing</em>
            <button class="pill-x" aria-label="Record the display instead" title="Record the display instead" onclick={() => setSource((x) => (x.kind = "display"))}>
              <svg viewBox="0 0 12 12" aria-hidden="true"><path d="M3 3l6 6M9 3l-6 6" /></svg>
            </button>
          </span>
        {/if}
        <span class="stat" class:on={armed} class:err={status?.phase === "error"}><i class="dot"></i>{phaseText(status)}</span>
        {#if armed && status}
          <span class="stat">{status.encoder}</span>
          <span class="stat">{status.resolution}</span>
          <span class="stat">{Math.min(status.buffered_secs, status.buffer_secs).toFixed(0)} / {status.buffer_secs} s</span>
        {/if}
        <span class="stat">{s.audio.enabled ? "Audio on" : "Audio off"}</span>
      </div>
      <TitleBar />
    </header>

    <main class="dash" bind:this={dashEl} use:dragscroll>
      {#if status?.settings_warning}
        <p class="banner warn" transition:slide={{ duration: 260, easing: cubicOut }}>{status.settings_warning}</p>
      {/if}
      {#each status?.notes ?? [] as note (note)}
        <p class="banner info" transition:slide={{ duration: 260, easing: cubicOut }}>{note}</p>
      {/each}
      {#if saveError}
        <p class="banner warn" role="alert" transition:slide={{ duration: 260, easing: cubicOut }}>Not saved: {saveError}</p>
      {/if}
      {#if actionError}
        <p class="banner warn" role="alert" transition:slide={{ duration: 260, easing: cubicOut }}>{actionError}</p>
      {/if}

    <div class="grid">
      <div class="stack">
      <section class="panel p-replay" id="replay">
        <div class="p-head"><h2>Replay</h2><span class="p-sub">Last {s.buffer_secs} s kept in memory</span></div>
          <!-- svelte-ignore a11y_no_static_element_interactions -->
          <section class="card status" class:on={armed} class:err={status?.phase === "error"} oncontextmenu={replayMenu}>
            <div class="status-top">
              <div>
                <div class="status-phase" class:shimmer={status?.phase === "starting" || status?.phase === "recovering"}><span class="dot"></span>{phaseText(status)}</div>
                <div class="status-detail">
                  {#if armed && status}
                    <span class="chips"><span class="chip">{status.encoder}</span><span class="chip">{status.resolution}</span></span>
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
            {#if armed && status}
              <div class="tiles">
                <div class="tile-stat"><span>Buffered</span><b>{Math.min(status.buffered_secs, status.buffer_secs).toFixed(0)} s</b><small>of {status.buffer_secs} s</small></div>
                <div class="tile-stat"><span>Memory</span><b>{status.buffered_mb.toFixed(0)} MB</b><small>in the buffer</small></div>
                <div class="tile-stat" title="The most a clip of the whole buffer can weigh at the highest bitrate of your quality preset, video only. Real clips are usually smaller."><span>Clip size</span><b>{fmtSize(status.buffer_estimate_mb || estimate)}</b><small>est. for {status.buffer_secs} s</small></div>
                <div class="tile-stat" class:bad={status.dropped > 0}><span>Dropped</span><b>{status.dropped}</b><small>frames</small></div>
              </div>
            {:else}
              <div class="meter-legend"><span>{status ? Math.min(status.buffered_secs, status.buffer_secs).toFixed(0) : 0} of {status?.buffer_secs ?? s.buffer_secs} s buffered</span></div>
            {/if}
            <div class="save-row">
              <div class="save-group">
                <span class="label">Save</span>
                <button class="btn" disabled={!armed} onclick={() => save(15)}>15 s</button>
                <button class="btn" disabled={!armed} onclick={() => save(30)}>30 s</button>
                <button class="btn" disabled={!armed} onclick={() => save(60)}>60 s</button>
                <button class="btn" disabled={!armed} onclick={() => save(null)}>Whole buffer</button>
                <span class="check-slot">
                  {#key savedFlash}
                    {#if savedFlash}<span class="check" aria-label="Clip saved">✓</span>{/if}
                  {/key}
                </span>
              </div>
              <div class="save-group more">
                <button class="btn ghost" disabled={!armed} onclick={() => act(api.screenshot)}>Screenshot</button>
                {#if status?.last_clip}
                  <button class="btn ghost" onclick={() => act(api.revealLastClip)}>Show last clip</button>
                {/if}
                <button class="btn ghost" onclick={() => act(api.openClipsFolder)}>Open folder</button>
              </div>
            </div>
          </section>

      </section>

      <section class="panel p-buffer">
        <div class="p-head"><h2>Buffer and files</h2></div>
          <section class="card">
            <div class="row">
              {@render row("Buffer length", `Up to ${estimate} MB of memory at the maximum bitrate.`)}
              <SliderField
                label="Buffer length in seconds"
                min={10}
                max={600}
                step={5}
                defaultValue={120}
                presets={[30, 60, 120, 300, 600]}
                value={s.buffer_secs}
                format={fmtBuffer}
                unitHint="seconds, or m:ss"
                parse={parseBuffer}
                onchange={(v) => edit((d) => (d.buffer_secs = v), 400)}
              />
            </div>
            <div class="row">
              {@render row("File format", "MP4 plays everywhere. MKV shows each audio track's name in players like mpv and VLC.")}
              <Seg
                label="File format"
                options={[
                  { value: "mp4", label: "MP4" },
                  { value: "mkv", label: "MKV" },
                ]}
                value={s.container}
                onselect={(c) => edit((d) => (d.container = c))}
              />
            </div>
            <!-- svelte-ignore a11y_no_static_element_interactions -->
            <div class="row" oncontextmenu={folderMenu}>
              {@render row("Clips folder", s.clips_dir ?? defaultDir)}
              <div class="inline">
                {#if s.clips_dir}
                  <button class="btn ghost" onclick={() => edit((d) => (d.clips_dir = null))}>Use default</button>
                {/if}
                <button class="btn" onclick={pickDir}>Change…</button>
              </div>
            </div>
          </section>
      </section>

      <section class="panel p-keys">
        <div class="p-head"><h2>Shortcuts</h2><span class="p-sub">Work in any app, including games</span></div>
          {#if status?.hotkey_errors.length}
            <p class="banner warn">
              Another app already uses: {status.hotkey_errors.join("; ")}. Pick a different combination.
            </p>
          {/if}
          <div class="sc-head">
            <span class="sc-count">{hotkeyRows.filter((r) => r.changed).length} of {hotkeyRows.length} changed from default</span>
            <span class="inline">
              <button class="btn ghost" disabled={hotkeyRows.every((r) => !r.value)} onclick={clearAllHotkeys}>Clear all</button>
              <button class="btn" disabled={hotkeyRows.every((r) => !r.changed)} onclick={resetAllHotkeys}>Reset all to default</button>
            </span>
          </div>
          <section class="card">
            {#each hotkeyRows as r (r.key)}
              <!-- svelte-ignore a11y_no_static_element_interactions -->
              <div class="row" class:conflict={r.clash} oncontextmenu={(e) => hotkeyMenu(e, r)}>
                <div class="row-text">
                  <div class="row-title">{r.label}</div>
                  <div class="row-desc">
                    {#if r.clash}
                      <span class="clash">Already used by “{r.clash}”. Pick another combination.</span>
                    {:else if r.changed}
                      Default: {r.def}
                    {:else}
                      Default shortcut
                    {/if}
                  </div>
                </div>
                <div class="sc-controls">
                  {#if r.changed}
                    <button class="icon-btn" title="Reset to {r.def}" aria-label="Reset {r.label} to default" onclick={() => resetHotkey(r.key)} transition:scale={{ duration: 260, start: 0.6, easing: backOut }}>
                      <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M3 12a9 9 0 1 0 3-6.7M3 4v5h5" /></svg>
                    </button>
                  {/if}
                  {#if r.value}
                    <button class="icon-btn" title="Clear shortcut" aria-label="Clear {r.label}" onclick={() => edit((d) => (d.hotkeys[r.key] = ""))} transition:scale={{ duration: 260, start: 0.6, easing: backOut }}>
                      <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6 6l12 12M18 6L6 18" /></svg>
                    </button>
                  {/if}
                  <HotkeyInput bind:this={hotkeyInputs[r.key]} value={r.value} onchange={(v) => edit((d) => (d.hotkeys[r.key] = v))} />
                </div>
              </div>
            {/each}
          </section>
          <p class="foot">Click a shortcut, then press the new keys. Backspace clears it, Escape keeps the old one.</p>
      </section>

      </div>

      <div class="stack">
      <section class="panel p-video" id="video">
        <div class="p-head"><h2>Video</h2><span class="p-sub">{presets.find((x) => x.id === s.preset)?.name}</span></div>
          <div class="source">
            <div class="src-head"><span class="src-title">Source</span><span class="src-line" class:live={target?.kind === "window"}>{sourceLine}</span></div>
            <Seg
              label="Video source"
              options={[
                { value: "display", label: "Display" },
                { value: "app", label: "Application" },
                { value: "auto", label: "Auto" },
              ]}
              value={src.kind}
              onselect={(k) => setSource((x) => (x.kind = k))}
            />
            {#if src.kind === "app"}
              <div class="src-body" transition:slide={{ duration: 260, easing: cubicOut }}>
                <Select
                  label="Application window"
                  options={windowOptions}
                  value={windowValue}
                  minWidth={0}
                  popupMin={380}
                  fill
                  onopen={loadWindows}
                  onselect={pickWindow}
                />
                {#if !windowOptions.length}<p class="src-note">No windows to record yet. Open the app, or add it by name below.</p>{/if}
                <div class="src-add">
                  <input
                    type="text"
                    placeholder="Add by name, e.g. game.exe"
                    spellcheck="false"
                    aria-label="Add an application by program name"
                    bind:value={nameText}
                    onkeydown={(e) => e.key === "Enter" && (e.preventDefault(), addByName())}
                  />
                  <button class="btn" disabled={!nameText.trim()} onclick={addByName}>Add</button>
                </div>
                {#if nameError}<p class="src-note warn" transition:slide={{ duration: 160 }}>{nameError}</p>{/if}
                <div class="src-row">
                  <span>Match this exact window<small>Only the window whose title contains "{src.title || "…"}"</small></span>
                  <Toggle on={exactOn} label="Match this exact window" onchange={setExact} />
                </div>
              </div>
            {:else if src.kind === "auto"}
              <p class="src-note" transition:slide={{ duration: 200 }}>Follows the game you play automatically (exclusive, borderless or windowed fullscreen, or a known game in a window), the display otherwise.</p>
            {/if}
            {#if src.kind !== "display"}
              <div class="src-row" transition:slide={{ duration: 200 }}>
                <span>If it is not running, record the display<small>Off keeps the last picture until the app is back.</small></span>
                <Toggle on={src.fallback} label="Record the display while the app is missing" onchange={(v) => setSource((x) => (x.fallback = v))} />
              </div>
            {/if}
          </div>
          <div class="presets">
            {#each presets as p (p.id)}
              <button class="preset" class:current={s.preset === p.id} onclick={() => edit((d) => (d.preset = p.id))}>
                <span class="preset-name">{p.name}</span>
                <span class="preset-desc">{p.desc}</span>
              </button>
            {/each}
          </div>
          {#if s.preset === "custom"}
            <section class="card" transition:slide={{ duration: 320, easing: cubicOut }}>
              <div class="row">
                {@render row("Resolution", "Video is scaled on the GPU and letterboxed to keep its shape.")}
                <Select
                  label="Resolution"
                  options={resolutions.map((r) => ({ value: `${r.w}x${r.h}`, label: r.label, tag: r.tag }))}
                  value={`${s.custom.width}x${s.custom.height}`}
                  onselect={(v) => {
                    const [w, h] = v.split("x").map(Number);
                    edit((d) => ((d.custom.width = w), (d.custom.height = h)));
                  }}
                />
              </div>
              <div class="row">
                {@render row("Frame rate", "Capped at the monitor's refresh rate.")}
                <Seg
                  label="Frame rate"
                  options={[30, 60, 120, 144, 240].map((f) => ({ value: f, label: String(f) }))}
                  value={s.custom.fps}
                  onselect={(f) => edit((d) => (d.custom.fps = f))}
                />
              </div>
              <div class="row">
                {@render row("Codec", codecUnavailable ? `${codecUnavailable} isn't supported by your GPU, so SuiCap uses Auto instead.` : support ? (support.problem ?? `${support.gpu}: ${support.codecs.map((c) => codecName[c]).join(", ")}. Auto picks the best of these.`) : "Auto picks AV1, then HEVC, then H.264, by what your GPU supports.")}
                <Seg
                  label="Codec"
                  options={codecOptions}
                  value={s.custom.codec}
                  onselect={(c) => edit((d) => (d.custom.codec = c))}
                />
              </div>
              <div class="row">
                {@render row("Quality", "Lower is sharper and bigger. 23 suits most games.")}
                <SliderField
                  label="Quality level"
                  min={15}
                  max={35}
                  hardMin={1}
                  hardMax={51}
                  defaultValue={23}
                  presets={[18, 21, 23, 26, 30]}
                  unitHint="1 to 51"
                  value={s.custom.quality}
                  onchange={(v) => edit((d) => (d.custom.quality = v), 400)}
                />
              </div>
            </section>
          {/if}
          <h2>Capture</h2>
          <section class="card">
            <div class="row">
              {@render row("Monitor", "Which screen to record.")}
              <Select
                label="Monitor"
                options={monitorOptions}
                value={s.display ?? 0}
                onselect={(n) => edit((d) => (d.display = n || null))}
              />
            </div>
            <div class="row">
              {@render row("Show mouse pointer", "")}
              <Toggle on={s.cursor} label="Show mouse pointer" onchange={(v) => edit((d) => (d.cursor = v))} />
            </div>
          </section>
      </section>

      </div>

      <section class="panel p-audio" id="audio">
        <div class="p-head"><h2>Audio</h2><span class="p-sub">Tracks and app routing</span></div>
        <AudioPage {s} {status} {edit} />
      </section>

      <section class="panel p-clips" id="clips">
        <div class="p-head"><h2>Clips</h2></div>
        <ClipsPage {s} {edit} />
      </section>
    </div>
  </main>
  </div>
</div>

{#if pending}
  <div class="apply-toast-wrap" transition:fly={{ y: 18, duration: 220, easing: cubicOut }}>
    <div class="banner confirm apply-toast" role="alert">
      <span>These changes restart capture and clear the {Math.round(status?.buffered_secs ?? 0)} s in the replay buffer.</span>
      <span class="inline">
        <button class="btn ghost" onclick={undo}>Undo</button>
        <button class="btn primary" onclick={persist}>Apply</button>
      </span>
    </div>
  </div>
{/if}

{#if settingsOpen}
  <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
  <div class="scrim" transition:fade={{ duration: 160 }} onclick={(e) => e.target === e.currentTarget && (settingsOpen = false)}>
    <div class="modal" role="dialog" aria-modal="true" aria-label="Settings" in:scale={{ duration: 260, start: 0.95, easing: backOut }} out:scale={{ duration: 140, start: 0.97 }}>
      <nav class="m-tabs" aria-label="Settings sections">
        <div class="m-head">Settings</div>
        {#each tabs as x (x.id)}
          <button class:current={tab === x.id} aria-current={tab === x.id ? "page" : undefined} onclick={() => (tab = x.id)}>
            <span class="tile" style="background: {x.tile}"><svg viewBox="0 0 24 24" aria-hidden="true"><path d={x.icon} /></svg></span>
            {x.label}
            {#if x.id === "updates" && upBadge}<span class="badge" aria-label="Update available"></span>{/if}
          </button>
        {/each}
      </nav>
      <div class="m-body" use:dragscroll>
        {#key tab}
          <div class="page">
            {#if tab === "general"}
          <h1>General</h1>
          <h2>Feedback</h2>
          <section class="card">
            <div class="row">
              {@render row("Sound when a clip is saved", "")}
              <Toggle on={s.save_sound} label="Sound when a clip is saved" onchange={(v) => edit((d) => (d.save_sound = v))} />
            </div>
            <div class="row">
              {@render row("Notification when a clip is saved", "Windows hides these while a game is full screen.")}
              <Toggle on={s.save_toast} label="Notification when a clip is saved" onchange={(v) => edit((d) => (d.save_toast = v))} />
            </div>
          </section>
          <h2>Startup</h2>
          <section class="card">
            <div class="row">
              {@render row("Start with Windows", "Opens in the tray, without this window.")}
              <Toggle on={s.autostart} label="Start with Windows" onchange={(v) => edit((d) => (d.autostart = v))} />
            </div>
            <div class="row">
              {@render row("Turn replay on at start", "")}
              <Toggle on={s.arm_on_start} label="Turn replay on at start" onchange={(v) => edit((d) => (d.arm_on_start = v))} />
            </div>
          </section>
          <h2>Data</h2>
          <section class="card">
            <div class="row">
              {@render row("Clips folder", "Where saved clips go.")}
              <button class="btn" onclick={() => act(api.openClipsFolder)}>Open</button>
            </div>
          </section>
          <p class="foot">Closing the window keeps SuiCap running in the tray. Quit from the tray menu.</p>
            {:else if tab === "appearance"}
          <h1>Appearance</h1>
          <section class="card">
            <div class="row">
              {@render row("Theme", "System follows your Windows setting.")}
              <Seg
                label="Theme"
                options={[
                  { value: "system", label: "System" },
                  { value: "light", label: "Light" },
                  { value: "dark", label: "Dark" },
                ]}
                value={s.theme}
                onselect={(t) => edit((d) => (d.theme = t))}
              />
            </div>
          </section>
          <h2>Recording badge</h2>
          <section class="card">
            <div class="row">
              {@render row("Show a badge while recording", "A small REC pill on the recorded monitor. It is hidden from clips and screenshots, and clicks pass through it.")}
              <Toggle on={s.recording_badge} label="Show a badge while recording" onchange={(v) => edit((d) => (d.recording_badge = v))} />
            </div>
            <div class="row" class:dim={!s.recording_badge}>
              {@render row("Also show while replay is on", "Otherwise it only appears during a recording.")}
              <Toggle on={s.badge_during_replay} label="Also show while replay is on" onchange={(v) => edit((d) => (d.badge_during_replay = v))} />
            </div>
            <div class="row" class:dim={!s.recording_badge}>
              {@render row("Corner", "Where it sits on the monitor.")}
              <div class="corners" role="radiogroup" aria-label="Badge corner">
                {#each corners as c (c.id)}
                  <button
                    class="corner {c.id}"
                    class:on={s.badge_corner === c.id}
                    role="radio"
                    aria-checked={s.badge_corner === c.id}
                    aria-label={c.label}
                    title={c.label}
                    onclick={() => edit((d) => (d.badge_corner = c.id))}
                  >
                    {#if s.badge_corner === c.id}
                      <span class="mini" class:rec={badgePreview.rec} class:ghost={badgePreview.sample || !s.recording_badge} style={miniStyle}>
                        {#if s.badge_icon !== "none" || !((s.badge_show_label && badgePreview.label.trim()) || s.badge_show_time)}
                          <span class="mini-icon"><BadgeIcon icon={s.badge_icon === "none" ? "dot" : s.badge_icon} src={customIcon} /></span>
                        {/if}
                        {#if s.badge_show_label && badgePreview.label.trim()}<b>{badgePreview.label}</b>{/if}
                        {#if s.badge_show_time}<em>{badgePreview.time}</em>{/if}
                      </span>
                    {/if}
                  </button>
                {/each}
              </div>
            </div>
          </section>
          <h2>Badge style</h2>
          <section class="card" class:dim={!s.recording_badge}>
            <div class="row">
              {@render row("Size", "Scales the whole badge.")}
              <SliderField label="Badge size" min={60} max={250} step={5} defaultValue={100} presets={[75, 100, 125, 150, 200]} value={s.badge_scale} format={(v) => `${v}%`} onchange={(v) => edit((d) => (d.badge_scale = Math.round(v)), 200)} />
            </div>
            <div class="row">
              {@render row("Opacity", "Of the badge background. The text stays solid.")}
              <SliderField label="Badge opacity" min={20} max={100} step={5} defaultValue={88} presets={[50, 75, 88, 100]} value={s.badge_opacity} format={(v) => `${v}%`} onchange={(v) => edit((d) => (d.badge_opacity = Math.round(v)), 200)} />
            </div>
            <div class="row">
              {@render row("Distance from the edge", "Gap between the badge and the monitor edge.")}
              <SliderField label="Distance from the edge in pixels" min={0} max={120} step={2} hardMax={400} defaultValue={16} presets={[0, 8, 16, 32, 64]} value={s.badge_margin} format={(v) => `${v} px`} onchange={(v) => edit((d) => (d.badge_margin = Math.round(v)), 200)} />
            </div>
            <div class="row">
              {@render row("Icon", "Shown first. It takes the recording or replay color.")}
              <div class="icons" role="radiogroup" aria-label="Badge icon">
                {#each BADGE_ICONS.filter((i) => i.id !== "none" && (i.id !== "custom" || customIcon)) as ic (ic.id)}
                  <button class="icon-pick" class:on={s.badge_icon === ic.id} role="radio" aria-checked={s.badge_icon === ic.id} aria-label={ic.label} title={ic.label} onclick={() => edit((d) => (d.badge_icon = ic.id))}>
                    {#if ic.id === "custom"}<img class="pick-img" src={customIcon} alt="" draggable="false" />{:else}<BadgeIcon icon={ic.id} />{/if}
                  </button>
                {/each}
                <span class="icon-gap" aria-hidden="true"></span>
                <button class="icon-pick" class:on={s.badge_icon === "none"} role="radio" aria-checked={s.badge_icon === "none"} aria-label="No icon" title="No icon" onclick={() => edit((d) => (d.badge_icon = "none"))}>
                  <span class="none">Off</span>
                </button>
              </div>
            </div>
            <div class="row">
              {@render row("Custom image", customIcon ? "Your image replaces the built-in icon. PNG, JPEG or WebP up to 1 MB; it keeps its own colors." : "Use your own image as the icon. PNG, JPEG or WebP up to 1 MB.")}
              <div class="custom-icon">
                {#if customIcon}<img src={customIcon} alt="" draggable="false" />{/if}
                <button class="btn" onclick={chooseIcon}>{customIcon ? "Replace…" : "Choose image…"}</button>
                {#if customIcon}<button class="btn ghost" onclick={removeIcon}>Remove</button>{/if}
              </div>
            </div>
            <div class="row">
              {@render row("Show the label", "Text next to the icon. It can differ while recording and while only replay is on.")}
              <Toggle on={s.badge_show_label} label="Show the label" onchange={(v) => edit((d) => (d.badge_show_label = v))} />
            </div>
            <div class="row" class:dim={!s.badge_show_label}>
              {@render row("Label text", "Up to 16 characters each. Leave it empty to hide the label in that state.")}
              <div class="labels">
                <label>Recording<input type="text" maxlength="16" value={s.badge_label_rec} disabled={!s.badge_show_label} spellcheck="false" oninput={(e) => edit((d) => (d.badge_label_rec = e.currentTarget.value), 250)} /></label>
                <label>Replay<input type="text" maxlength="16" value={s.badge_label_replay} disabled={!s.badge_show_label} spellcheck="false" oninput={(e) => edit((d) => (d.badge_label_replay = e.currentTarget.value), 250)} /></label>
              </div>
            </div>
            <div class="row">
              {@render row("Show the timer", "The recording time, or the buffered seconds while only replay is on.")}
              <Toggle on={s.badge_show_time} label="Show the timer" onchange={(v) => edit((d) => (d.badge_show_time = v))} />
            </div>
            <div class="row">
              {@render row("Announce saved clips", "When a clip is saved, the badge briefly says so and shows its length, even if the label is off. While the badge is showing, the Windows notification is skipped.")}
              <Toggle on={s.badge_clip_flash} label="Announce saved clips" onchange={(v) => edit((d) => (d.badge_clip_flash = v))} />
            </div>
            <div class="row">
              {@render row("Colors", "Background, text, the recording dot and the replay dot.")}
              <div class="swatches">
                {#each badgeColors as c (c.key)}
                  <label class="swatch" title={c.label}>
                    <input type="color" value={s[c.key]} aria-label={c.label} oninput={(e) => edit((d) => (d[c.key] = e.currentTarget.value), 120)} />
                    <span>{c.short}</span>
                  </label>
                {/each}
              </div>
            </div>
            <div class="row">
              {@render row("Reset badge style", "Puts the size, opacity, edge distance, icon, label, timer and colors back to their defaults. The corner and any image you uploaded are kept.")}
              <button class="btn" onclick={resetBadgeStyle}>Reset to defaults</button>
            </div>
          </section>
            {:else if tab === "updates"}
          <h1>Updates</h1>
          <section class="card status" class:on={upBadge} class:err={upState?.phase === "failed"}>
            <div class="status-top">
              <div>
                <div class="status-phase"><span class="dot"></span>{upHead.title}</div>
                <div class="status-detail">{upHead.desc}</div>
              </div>
              <div class="inline">
                {#if upState?.phase === "available"}
                  <button class="btn" onclick={() => act(api.updateDownload)}>Download</button>
                  <button class="btn primary" onclick={() => act(api.updateInstall)}>Install and restart</button>
                {:else if upState?.phase === "ready"}
                  <button class="btn primary" onclick={() => act(api.updateInstall)}>Install and restart</button>
                {:else if upState?.phase === "failed" && upState.version}
                  <button class="btn primary" onclick={() => act(api.updateInstall)}>Retry</button>
                {:else}
                  <button class="btn" disabled={upBusy} onclick={() => act(api.updateCheck)}>Check for updates</button>
                {/if}
              </div>
            </div>
            {#if upBusy}
              <div class="meter big busy"><span></span></div>
            {/if}
          </section>
          <h2>Preferences</h2>
          <section class="card">
            <div class="row">
              {@render row("Automatic updates", updateModeDesc)}
              <Seg
                label="Automatic updates"
                options={[...updateModes]}
                value={s.update_mode}
                onselect={(m) => edit((d) => (d.update_mode = m))}
              />
            </div>
            <div class="row">
              {@render row("Last checked", lastChecked)}
            </div>
          </section>
          <h2>What's new</h2>
          {#each logs as entry (entry.title)}
            <section class="card changelog">
              <h3>{entry.title}</h3>
              <!-- eslint-disable-next-line svelte/no-at-html-tags -- the renderer escapes all HTML first -->
              {@html entry.html}
            </section>
          {/each}
          {#if changelog.length > 3}
            <button class="btn" onclick={() => (showAllLogs = !showAllLogs)}>
              {showAllLogs ? "Show fewer" : `Show ${changelog.length - 3} older`}
            </button>
          {/if}
          <p class="foot">Updates come from SuiCap's GitHub releases. Each installer is checked against its published checksum before it runs. Automatic installs never interrupt a recording.</p>
            {:else}
          <h1>About</h1>
          <section class="card about">
            <svg class="about-mark" viewBox="0 0 256 256" aria-hidden="true"><path d="M48.3 36.0L76.3 52.17V203.83L48.3 220.0Z" fill="currentColor" /><path d="M88.3 59.09L116.3 75.26V180.74L88.3 196.91Z" fill="currentColor" /><path d="M128.3 82.19L156.3 98.35V157.65L128.3 173.81Z" fill="currentColor" /><path d="M168.3 105.28L207.7 128L168.3 150.72Z" fill="var(--accent)" /></svg>
            <div>
              <div class="about-name">SuiCap</div>
              <div class="about-sub">Version {appInfo?.version ?? ""} · by {appInfo?.developer ?? "CafeTako"}</div>
              <div class="about-sub">GPU replay buffer and multi-track capture for Windows.</div>
            </div>
          </section>
          <h2>Developer</h2>
          <section class="card">
            <div class="row">
              {@render row("Made by", "")}
              <span class="value">{appInfo?.developer ?? "CafeTako"}</span>
            </div>
            <div class="row">
              {@render row("Support", appInfo?.email ?? "support@cafetako.com")}
              <button class="btn" onclick={() => act(() => api.openLink("support"))}>Email us</button>
            </div>
            <div class="row">
              {@render row("Support the developer", "SuiCap is free. A coffee helps keep it growing.")}
              <button class="btn" onclick={() => act(() => api.openLink("coffee"))}>Buy me a coffee</button>
            </div>
          </section>
          <h2>Your data</h2>
          <section class="card">
            <div class="row">
              {@render row("Stored only on this PC", "Settings and clips stay on your machine. No accounts, no tracking.")}
              <button class="btn" onclick={() => act(api.openClipsFolder)}>Open clips folder</button>
            </div>
            <div class="row">
              {@render row("Updates", upHead.title)}
              <button class="btn" onclick={() => (tab = "updates")}>Open</button>
            </div>
          </section>
            {/if}
          </div>
        {/key}
      </div>
    </div>
  </div>
{/if}

<ContextMenu />

<style>
  .shell {
    display: grid;
    grid-template-rows: 1fr;
    grid-template-columns: 76px minmax(0, 1fr);
    height: 100vh;
  }

  .dot {
    display: inline-block;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--text-3);
    flex: none;
    transition: background var(--med) var(--ease);
  }
  .on .dot {
    background: var(--accent);
    animation: pulse 2.4s var(--ease) infinite;
  }
  .err .dot {
    background: var(--warn);
  }
  @keyframes pulse {
    0% {
      box-shadow: 0 0 0 0 var(--accent-soft);
    }
    70%,
    100% {
      box-shadow: 0 0 0 7px transparent;
    }
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
    border-radius: 2px;
    background: var(--text-3);
    transition:
      width 600ms linear,
      background var(--med) var(--ease);
  }
  .on .meter span {
    background: var(--accent);
  }
  .meter.big {
    height: 6px;
    margin-top: 18px;
    border-radius: 3px;
  }

  /* Filter chips (Site Builder "filter chips"). */
  .chips {
    display: inline-flex;
    flex-wrap: wrap;
    gap: 6px;
  }
  .chip {
    padding: 2px 10px;
    border-radius: 999px;
    background: var(--surface-3);
    font-size: 12px;
    font-weight: 550;
  }

  /* Stat tiles (Site Builder "stat tiles"). */
  .tiles {
    display: grid;
    grid-template-columns: repeat(4, 1fr);
    gap: 8px;
    margin-top: 14px;
  }
  .tile-stat {
    display: grid;
    gap: 1px;
    padding: 10px 12px;
    border-radius: var(--radius-md);
    background: var(--surface-2);
    font-variant-numeric: tabular-nums;
    animation: rise var(--slow) var(--spring) both;
  }
  .tile-stat span,
  .tile-stat small {
    color: var(--text-3);
    font-size: 11.5px;
    font-weight: 550;
  }
  .tile-stat b {
    font: 650 18px var(--font-display);
  }
  .tile-stat.bad b {
    color: var(--warn);
  }

  /* Success check (Site Builder "check-pop"). */
  .check {
    display: grid;
    place-items: center;
    width: 24px;
    height: 24px;
    margin-left: 6px;
    border-radius: 50%;
    background: var(--ok);
    color: #04210f;
    font-size: 13px;
    font-weight: 800;
    animation: check-pop var(--slow) var(--spring) both;
  }
  @keyframes check-pop {
    from {
      opacity: 0;
      transform: scale(0.3) rotate(-20deg);
    }
  }

  /* Shimmer heading (Site Builder "sheen-sweep-text"). */
  .shimmer {
    background: linear-gradient(100deg, var(--text-3) 35%, var(--text) 50%, var(--text-3) 65%) 0 0 / 250% 100%;
    -webkit-background-clip: text;
    background-clip: text;
    color: transparent;
    animation: sheen 2.4s linear infinite;
  }
  @keyframes sheen {
    to {
      background-position: -150% 0;
    }
  }

  .page > :global(*) {
    animation: rise var(--slow) var(--spring) both;
  }
  .page > :global(:nth-child(2)) { animation-delay: 40ms; }
  .page > :global(:nth-child(3)) { animation-delay: 80ms; }
  .page > :global(:nth-child(4)) { animation-delay: 120ms; }
  .page > :global(:nth-child(5)) { animation-delay: 160ms; }
  .page > :global(:nth-child(n + 6)) { animation-delay: 200ms; }
  @keyframes rise {
    from {
      opacity: 0;
      transform: translateY(10px);
    }
  }

  h1 {
    margin: 0 0 18px;
    font: 650 24px var(--font-display);
    letter-spacing: -0.01em;
  }
  h2 {
    margin: 20px 0 8px 4px;
    color: var(--text-3);
    font: 600 12px var(--font);
    letter-spacing: 0.04em;
    text-transform: uppercase;
  }
  .changelog {
    padding: 18px 22px 8px;
    font-size: 13px;
    line-height: 1.55;
  }
  .changelog h3 {
    margin: 0 0 4px;
    font: 650 15px var(--font-display);
  }
  .changelog :global(h4) {
    margin: 14px 0 6px;
    color: var(--ok);
    font-size: 11.5px;
    font-weight: 700;
    letter-spacing: 0.06em;
    text-transform: uppercase;
  }
  .changelog :global(ul) {
    margin: 0 0 12px;
    padding-left: 18px;
  }
  .changelog :global(li) {
    margin: 0 0 8px;
    padding-left: 2px;
  }
  .changelog :global(li)::marker {
    color: var(--text-3);
  }
  .changelog :global(p) {
    margin: 0 0 12px;
  }
  .changelog :global(strong) {
    font-weight: 650;
  }
  .changelog :global(code) {
    padding: 1px 6px;
    border-radius: 6px;
    background: var(--surface-3);
    font: 500 12px ui-monospace, Consolas, monospace;
  }
  .badge {
    width: 8px;
    height: 8px;
    margin-left: auto;
    border-radius: 50%;
    background: var(--accent);
  }
  .about {
    display: flex;
    align-items: center;
    gap: 20px;
    padding: 22px;
  }
  .about-mark {
    width: 72px;
    height: 72px;
    flex: none;
    color: var(--text);
  }
  .about-name {
    font: 700 24px var(--font-display);
  }
  .about-sub {
    margin-top: 3px;
    color: var(--text-3);
    font-size: 12.5px;
    font-weight: 500;
  }
  .value {
    font-size: 13px;
    font-weight: 600;
  }
  .meter.busy {
    overflow: hidden;
  }
  .meter.busy span {
    width: 28%;
    animation: slide-bar 1.4s var(--ease) infinite;
  }
  @keyframes slide-bar {
    from {
      transform: translateX(-100%);
    }
    to {
      transform: translateX(360%);
    }
  }
  .foot {
    margin: 4px 4px 0;
    color: var(--text-3);
    font-size: 12.5px;
  }

  .card {
    margin-bottom: 14px;
    border-radius: var(--radius);
    background: var(--surface);
    box-shadow: inset 0 0 0 1px var(--line);
    transition: opacity var(--med) var(--ease);
  }
  .status {
    padding: 20px 22px;
  }
  .status.on {
    background: linear-gradient(135deg, var(--accent-soft), transparent 60%), var(--surface);
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
    font: 650 18px var(--font-display);
  }
  .status-detail {
    margin-top: 4px;
    color: var(--text-2);
    font-size: 13px;
  }
  .meter-legend {
    display: flex;
    justify-content: space-between;
    margin-top: 7px;
    color: var(--text-3);
    font-size: 12px;
    font-variant-numeric: tabular-nums;
  }
  .save-row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px;
    margin-top: 18px;
    padding-top: 16px;
    border-top: 1px solid var(--line);
  }
  .save-row .label {
    margin-right: 4px;
    color: var(--text-2);
    font-size: 13px;
  }
  .save-group {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  /* Wraps as one unit, right-aligned, instead of stranding a single button. */
  .save-group.more {
    margin-left: auto;
  }
  /* The check's room is always reserved, so it never pushes the row to wrap. */
  .check-slot {
    display: grid;
    place-items: center;
    width: 30px;
    height: 24px;
  }
  .check-slot .check {
    margin-left: 0;
  }

  .row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 24px;
    padding: 14px 18px;
    min-height: 60px;
    transition: opacity var(--med) var(--ease);
  }
  .row + .row {
    border-top: 1px solid var(--line);
  }
  .sc-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    margin: 0 4px 10px;
  }
  .sc-count {
    color: var(--text-3);
    font-size: 12.5px;
  }
  .sc-controls {
    display: flex;
    align-items: center;
    gap: 6px;
    flex: none;
  }
  .icon-btn {
    display: grid;
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
      color var(--fast) var(--ease),
      transform var(--med) var(--spring);
  }
  .icon-btn:hover {
    background: var(--surface-3);
    color: var(--text);
  }
  .icon-btn:active {
    transform: scale(0.88);
  }
  .icon-btn svg {
    width: 15px;
    height: 15px;
    fill: none;
    stroke: currentColor;
    stroke-width: 2;
    stroke-linecap: round;
    stroke-linejoin: round;
  }
  .row.conflict {
    background: var(--warn-soft);
  }
  .clash {
    color: var(--warn);
  }
  .row.dim {
    opacity: 0.5;
  }
  .corners {
    position: relative;
    width: 208px;
    height: 118px;
    flex: none;
    border-radius: 8px;
    background: var(--surface-3);
    box-shadow: inset 0 0 0 1.5px var(--line-strong);
  }
  .corner {
    position: absolute;
    display: flex;
    align-items: center;
    min-width: 34px;
    height: 14px;
    padding: 0;
    border: 0;
    border-radius: 999px;
    background: var(--surface-2);
    cursor: pointer;
    transition:
      background var(--fast) var(--ease),
      height var(--med) var(--ease),
      transform var(--med) var(--spring);
  }
  .corner:hover {
    background: var(--line-strong);
  }
  .corner:active {
    transform: scale(0.94);
  }
  .corner.top-left {
    top: 8px;
    left: 8px;
  }
  .corner.top-right {
    top: 8px;
    right: 8px;
  }
  .corner.bottom-left {
    bottom: 8px;
    left: 8px;
  }
  .corner.bottom-right {
    bottom: 8px;
    right: 8px;
  }
  /* The live badge, drawn like ui/Badge.svelte at about 80% size. */
  .swatches {
    display: flex;
    align-items: center;
    gap: 10px;
    flex: none;
  }
  .swatch {
    display: grid;
    justify-items: center;
    gap: 4px;
    color: var(--text-3);
    font-size: 11px;
    cursor: pointer;
  }
  .swatch input {
    width: 34px;
    height: 24px;
    padding: 0;
    border: 0;
    border-radius: 6px;
    background: none;
    cursor: pointer;
  }
  .swatch input::-webkit-color-swatch-wrapper {
    padding: 0;
  }
  .swatch input::-webkit-color-swatch {
    border: 0;
    border-radius: 6px;
    box-shadow: inset 0 0 0 1px var(--line-strong);
  }
  .corner.on {
    height: auto;
    background: transparent;
  }
  .mini {
    display: flex;
    align-items: center;
    gap: 0.66em;
    height: 2.6em;
    padding: 0 0.95em;
    border-radius: 999px;
    background: color-mix(in srgb, var(--mb, #121317) var(--ma, 88%), transparent);
    box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--mf, #f2f2f4) 14%, transparent);
    color: var(--mf, #f2f2f4);
    font-size: calc(10px * var(--ms, 1));
    white-space: nowrap;
    animation: mini-in 220ms var(--ease) both;
  }
  .mini.ghost {
    opacity: 0.55;
  }
  .mini b {
    font-weight: 650;
    letter-spacing: 0.08em;
  }
  .mini em {
    margin-left: 0;
    color: color-mix(in srgb, var(--mf, #f2f2f4) 68%, transparent);
    font-style: normal;
    font-variant-numeric: tabular-nums;
  }
  .custom-icon {
    display: flex;
    align-items: center;
    gap: 8px;
    flex: none;
  }
  .custom-icon img {
    width: 32px;
    height: 32px;
    border-radius: 6px;
    object-fit: contain;
    background: var(--surface-3);
    box-shadow: inset 0 0 0 1px var(--line-strong);
  }
  .icons {
    display: flex;
    flex-wrap: nowrap;
    justify-content: flex-end;
    gap: 6px;
    flex: none;
  }
  /* Sets "no icon" apart from the icons you can use. */
  .icon-gap {
    width: 1px;
    height: 20px;
    margin: 0 6px;
    background: var(--line-strong);
  }
  .icon-pick {
    display: grid;
    place-items: center;
    width: 36px;
    height: 32px;
    padding: 0;
    border: 0;
    border-radius: var(--radius-sm);
    background: var(--surface-2);
    color: var(--text-2);
    font-size: 17px;
    cursor: pointer;
    transition:
      background var(--fast) var(--ease),
      color var(--fast) var(--ease),
      transform var(--med) var(--spring);
  }
  .icon-pick:hover {
    background: var(--surface-3);
  }
  .icon-pick:active {
    transform: scale(0.92);
  }
  .icon-pick.on {
    background: var(--accent-soft);
    color: var(--accent);
    box-shadow: inset 0 0 0 1.5px var(--accent);
  }
  .icon-pick .pick-img {
    width: 22px;
    height: 22px;
    border-radius: 4px;
    object-fit: contain;
  }
  .icon-pick .none {
    font-size: 11px;
    font-weight: 600;
  }
  .labels {
    display: flex;
    gap: 10px;
    flex: none;
  }
  .labels label {
    display: grid;
    gap: 4px;
    color: var(--text-3);
    font-size: 11px;
  }
  .labels input {
    width: 112px;
    height: 30px;
    padding: 0 9px;
    border: 0;
    border-radius: var(--radius-sm);
    background: var(--surface-3);
    color: var(--text);
    font: 500 13px var(--font);
  }
  .labels input:disabled {
    opacity: 0.5;
  }
  .mini-icon {
    display: inline-flex;
    font-size: 1.05em;
    color: var(--mrep, #3ba7ff);
  }
  .mini.rec .mini-icon {
    color: var(--mrec, #ef4444);
    animation: mini-pulse 1.6s ease-in-out infinite;
  }
  @keyframes mini-pulse {
    50% {
      opacity: 0.35;
    }
  }
  @keyframes mini-in {
    from {
      opacity: 0;
      transform: scale(0.9);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .mini.rec .mini-icon {
      animation: none;
    }
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
    overflow-wrap: anywhere;
  }

  /* ---- Controls ---- */
  .btn {
    height: 32px;
    padding: 0 15px;
    border: 0;
    border-radius: var(--radius-sm);
    background: var(--surface-3);
    font-size: 12.5px;
    font-weight: 600;
    cursor: pointer;
    transition:
      background var(--fast) var(--ease),
      transform var(--med) var(--spring),
      opacity var(--fast) var(--ease);
  }
  .btn:hover:not(:disabled) {
    background: var(--line-strong);
  }
  .btn:active:not(:disabled) {
    transform: scale(0.96);
  }
  .btn:disabled {
    opacity: 0.4;
    cursor: default;
  }
  .btn.primary {
    background: var(--accent);
    color: var(--on-accent);
  }
  .btn.primary:hover:not(:disabled) {
    background: var(--accent-strong);
  }
  .btn.ghost {
    background: transparent;
    color: var(--text-2);
  }
  .btn.ghost:hover:not(:disabled) {
    background: var(--surface-2);
    color: var(--text);
  }
  .inline {
    display: flex;
    gap: 6px;
    flex: none;
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
    padding: 15px 16px;
    border: 0;
    border-radius: var(--radius);
    background: var(--surface);
    box-shadow: inset 0 0 0 1px var(--line);
    text-align: left;
    cursor: pointer;
    transition:
      background var(--fast) var(--ease),
      box-shadow var(--med) var(--ease),
      transform var(--med) var(--spring);
  }
  .preset:hover {
    background: var(--surface-2);
    transform: translateY(-2px);
  }
  .preset:active {
    transform: scale(0.98);
  }
  .preset.current {
    background: var(--accent-soft);
    box-shadow: inset 0 0 0 1.5px var(--accent);
  }
  .preset-name {
    font-weight: 650;
  }
  .preset-desc {
    color: var(--text-2);
    font-size: 12px;
  }

  /* Phase-banner badge (Site Builder "phase and notice banner"). */
  .banner {
    display: flex;
    align-items: center;
    gap: 12px;
  }
  .banner::before {
    flex: none;
    padding: 2px 8px;
    border-radius: 6px;
    font: 700 10.5px var(--font);
    letter-spacing: 0.05em;
    text-transform: uppercase;
  }
  .banner.info::before {
    content: "Note";
    background: var(--surface-3);
    color: var(--text-2);
  }
  .banner.warn::before {
    content: "Heads up";
    background: var(--warn);
    color: #1a1204;
  }
  .banner.confirm::before {
    content: "Restart";
    background: var(--accent);
    color: var(--on-accent);
  }
  .banner.confirm > span:first-of-type {
    flex: 1;
  }
  .banner {
    margin: 0 0 12px;
    padding: 11px 15px;
    border-radius: var(--radius-md);
    font-size: 13px;
  }
  .banner.confirm {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    background: var(--accent-soft);
    box-shadow: inset 0 0 0 1px var(--accent);
  }
  .apply-toast-wrap {
    position: fixed;
    left: 0;
    right: 0;
    bottom: 20px;
    z-index: 60;
    display: flex;
    justify-content: center;
    padding: 0 20px;
    pointer-events: none;
  }
  .banner.confirm.apply-toast {
    pointer-events: auto;
    margin: 0;
    max-width: min(680px, 100%);
    background: color-mix(in srgb, var(--accent-soft) 92%, var(--surface));
    backdrop-filter: blur(14px);
    box-shadow:
      inset 0 0 0 1px var(--accent),
      0 12px 32px rgba(0, 0, 0, 0.36);
  }
  .banner.info {
    background: var(--surface);
    color: var(--text-2);
  }
  .banner.warn {
    background: var(--warn-soft);
    box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--warn) 40%, transparent);
    color: var(--text);
  }

  /* ---- Settings button and modal ---- */
  .m-tabs .tile {
    display: grid;
    place-items: center;
    width: 24px;
    height: 24px;
    border-radius: 7px;
    color: #fff;
  }
  .m-tabs .tile svg {
    width: 14px;
    height: 14px;
    fill: none;
    stroke: currentColor;
    stroke-width: 2;
    stroke-linecap: round;
    stroke-linejoin: round;
  }
  .scrim {
    position: fixed;
    inset: 0;
    z-index: 500;
    display: grid;
    place-items: center;
    padding: 24px;
    background: rgba(0, 0, 0, 0.5);
    backdrop-filter: blur(6px);
  }
  .modal {
    display: grid;
    grid-template-columns: 210px 1fr;
    width: min(940px, 100%);
    height: min(640px, 100%);
    overflow: hidden;
    border-radius: 18px;
    background: var(--surface-solid);
    box-shadow:
      inset 0 0 0 1px var(--line-strong),
      var(--shadow-menu);
  }
  .m-tabs {
    display: flex;
    flex-direction: column;
    gap: 3px;
    padding: 18px 10px;
    border-right: 1px solid var(--line);
    background: var(--sidebar);
  }
  .m-head {
    padding: 2px 10px 12px;
    font: 650 15px var(--font-display);
  }
  .m-tabs button {
    position: relative;
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 7px 10px;
    border: 0;
    border-radius: 10px;
    background: transparent;
    color: var(--text-2);
    font-size: 13px;
    font-weight: 600;
    text-align: left;
    cursor: pointer;
    transition:
      background var(--fast) var(--ease),
      color var(--fast) var(--ease);
  }
  .m-tabs button:hover {
    background: var(--surface-2);
  }
  .m-tabs button.current {
    background: var(--surface-3);
    color: var(--text);
  }
  .m-body {
    position: relative;
    overflow-x: hidden;
    overflow-y: auto;
    padding: 6px 28px 28px;
  }

  /* ---- Dashboard: one screen, divided into panels ---- */
  .main-col {
    grid-row: 1;
    grid-column: 2;
    display: grid;
    grid-template-rows: auto minmax(0, 1fr);
    min-width: 0;
    min-height: 0;
  }
  .dash:global(.grabbing),
  .m-body:global(.grabbing) {
    cursor: grabbing;
    user-select: none;
  }
  .dash {
    min-width: 0;
    overflow-x: hidden;
    overflow-y: auto;
    padding: 0 28px 36px;
  }
  .top {
    display: flex;
    align-items: center;
    gap: 22px;
    padding: 12px 10px 18px 28px;
    user-select: none;
  }
  .word {
    display: flex;
    align-items: center;
    height: 42px;
    font: 700 34px/1 var(--font-display);
    letter-spacing: -0.02em;
  }
  .stats {
    display: flex;
    flex: 1;
    flex-wrap: wrap;
    align-items: center;
    justify-content: center;
    gap: 8px;
    min-width: 0;
  }
  .stat {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    padding: 5px 12px;
    border-radius: 999px;
    background: var(--surface);
    box-shadow: inset 0 0 0 1px var(--line);
    color: var(--text-2);
    font-size: 12.5px;
    font-weight: 550;
    font-variant-numeric: tabular-nums;
  }
  .stat.game .pill-act {
    margin: 0 -4px 0 2px;
    padding: 1px 8px;
    border: 0;
    border-radius: 999px;
    background: var(--accent);
    color: var(--on-accent);
    font-size: 11px;
    font-weight: 650;
    cursor: pointer;
    opacity: 0;
    transition:
      opacity var(--fast) var(--ease),
      transform var(--med) var(--spring);
  }
  .stat.game:hover .pill-act,
  .stat.game .pill-act:focus-visible,
  .stat.game .pill-act.always {
    opacity: 1;
  }
  .stat.game .pill-act:active {
    transform: scale(0.94);
  }
  .stat.game em {
    color: var(--accent);
    font-size: 11px;
    font-style: normal;
    font-weight: 700;
    letter-spacing: 0.04em;
    text-transform: uppercase;
  }
  .stat.game.capturing {
    background: var(--accent);
    color: var(--on-accent);
  }
  .stat.game.capturing em {
    color: inherit;
    opacity: 0.8;
  }
  .pill-x {
    display: grid;
    place-items: center;
    width: 18px;
    height: 18px;
    margin: 0 -4px 0 0;
    padding: 0;
    border: 0;
    border-radius: 50%;
    background: color-mix(in srgb, currentColor 18%, transparent);
    color: inherit;
    cursor: pointer;
    transition: background var(--fast) var(--ease);
  }
  .pill-x svg {
    width: 10px;
    height: 10px;
    fill: none;
    stroke: currentColor;
    stroke-width: 1.8;
    stroke-linecap: round;
  }
  .pill-x:hover {
    background: color-mix(in srgb, currentColor 32%, transparent);
  }
  .stat.game {
    padding-left: 7px;
    background: var(--accent-soft);
    box-shadow: inset 0 0 0 1px var(--accent);
    color: var(--text);
  }
  .grid {
    display: grid;
    grid-template-columns: minmax(0, 1fr) 380px;
    gap: 16px;
    align-items: stretch;
  }
  .panel {
    scroll-margin-top: 12px;
    min-width: 0;
    padding: 18px 20px 20px;
    border-radius: 22px;
    background: var(--surface);
    box-shadow: inset 0 0 0 1px var(--line);
    animation: rise var(--slow) var(--spring) both;
  }
  .p-audio,
  .p-clips {
    grid-column: 1 / -1;
  }
  .panel:nth-child(2) { animation-delay: 50ms; }
  .panel:nth-child(3) { animation-delay: 100ms; }
  .panel:nth-child(4) { animation-delay: 150ms; }
  .panel:nth-child(n + 5) { animation-delay: 200ms; }
  .p-head {
    display: flex;
    align-items: baseline;
    gap: 12px;
    margin: 0 0 14px;
  }
  .p-head h2 {
    margin: 0;
    color: var(--text);
    font: 650 17px var(--font-display);
    letter-spacing: -0.01em;
    text-transform: none;
  }
  .p-sub {
    color: var(--text-3);
    font-size: 12px;
    font-weight: 500;
  }
  /* The old page cards sit flat inside a panel. */
  .panel .card {
    margin: 0;
    background: transparent;
    box-shadow: none;
  }
  .panel .card.status {
    padding: 0;
  }
  .panel .card:not(.status) {
    margin-top: 2px;
  }
  .panel > h2 {
    margin: 14px 0 8px;
  }
  .panel .presets {
    grid-template-columns: 1fr 1fr;
    gap: 8px;
  }
  .panel .preset {
    padding: 11px 13px;
    border-radius: 14px;
    background: var(--surface-2);
    box-shadow: inset 0 0 0 1.5px transparent;
  }
  .panel .preset.current {
    background: var(--accent-soft);
    box-shadow: inset 0 0 0 1.5px var(--accent);
  }
  .panel .tiles {
    margin-top: 12px;
  }

  .stack > .panel:last-child {
    flex: 1;
  }
  .stack {
    display: flex;
    min-width: 0;
    flex-direction: column;
    gap: 16px;
  }
  .source {
    display: flex;
    flex-direction: column;
    gap: 10px;
    margin-bottom: 14px;
    padding: 12px 13px;
    border-radius: 14px;
    background: var(--surface-2);
  }
  .src-head {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    justify-content: space-between;
    gap: 4px 10px;
  }
  .src-title {
    font: 650 13px var(--font-display);
  }
  .src-line {
    color: var(--text-3);
    font-size: 12px;
    font-weight: 550;
  }
  .src-line.live {
    color: var(--accent);
  }
  .src-body {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .src-note {
    margin: 0;
    color: var(--text-3);
    font-size: 12px;
    line-height: 1.4;
  }
  .src-note.warn {
    color: var(--warn);
  }
  .src-add {
    display: flex;
    gap: 6px;
  }
  .src-add input {
    flex: 1;
    min-width: 0;
    height: 34px;
    padding: 0 11px;
    border: 0;
    border-radius: var(--radius-sm);
    outline: 0;
    background: var(--surface-3);
    box-shadow: inset 0 0 0 1.5px transparent;
    font-size: 12.5px;
    user-select: text;
    transition: box-shadow var(--med) var(--ease);
  }
  .src-add input:focus {
    box-shadow: inset 0 0 0 1.5px var(--accent);
  }
  .src-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    font-size: 12.5px;
    font-weight: 550;
  }
  .src-row small {
    display: block;
    margin-top: 1px;
    color: var(--text-3);
    font-size: 11.5px;
    font-weight: 500;
  }
  .p-video .row {
    flex-direction: column;
    align-items: stretch;
    gap: 8px;
  }
  /* Full-width button groups in this narrow column: equal buttons that fill the row. */
  .p-video :global(.seg) {
    display: flex;
  }
  .p-video :global(.seg > button) {
    flex: 1 1 0;
    min-width: 0;
    padding: 0 6px;
  }
</style>
