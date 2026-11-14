<script lang="ts">
  // The recording badge (src-tauri/src/badge.rs): a small pill in a monitor corner, styled by the
  // user's badge settings. The shell excludes this window from capture and makes it
  // click-through; this page reports its own size so the window is exactly as wide as the text.
  import { onMount } from "svelte";
  import { api, onBadgeIcon, onClipFlash, onScreenshotFlash, onSettings, onStatus, type Settings, type Status } from "./api";
  import BadgeIcon from "./BadgeIcon.svelte";

  let status = $state<Status | null>(null);
  let cfg = $state<Settings | null>(null);
  let pill = $state<HTMLDivElement>();
  let iconSrc = $state<string | null>(null);

  const recording = $derived(Boolean(status?.recording));
  const clock = (t: number) => {
    const s = Math.max(0, Math.floor(t));
    return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
  };
  // A saved clip or screenshot takes over the label (and timer for clips) for a few seconds.
  type Flash = { kind: "clip"; secs: number } | { kind: "screenshot" };
  let flash = $state<Flash | null>(null);
  let flashTimer: ReturnType<typeof setTimeout> | undefined;
  function triggerFlash(next: Flash) {
    flash = next;
    clearTimeout(flashTimer);
    flashTimer = setTimeout(() => (flash = null), 3500);
  }
  const flashing = $derived(flash !== null && Boolean(cfg?.badge_clip_flash));
  const time = $derived(
    flashing
      ? flash?.kind === "clip"
        ? `${Math.max(1, Math.round(flash.secs))} s`
        : ""
      : recording
        ? clock(status?.recording_secs ?? 0)
        : `${Math.round(status?.buffered_secs ?? 0)} s`,
  );
  const label = $derived(
    flashing
      ? flash?.kind === "screenshot"
        ? "SCREENSHOT SAVED"
        : "CLIP SAVED"
      : ((recording ? cfg?.badge_label_rec : cfg?.badge_label_replay) ?? ""),
  );
  const showLabel = $derived(flashing || Boolean(cfg?.badge_show_label && label.trim()));
  const showTime = $derived(flashing ? flash?.kind === "clip" : Boolean(cfg?.badge_show_time));
  const showIcon = $derived(cfg?.badge_icon !== "none");
  /** With everything switched off the badge would be empty, so the dot stays. */
  const fallbackIcon = $derived(!showIcon && !showLabel && !showTime);
  const style = $derived(
    cfg
      ? `--s:${cfg.badge_scale / 100};--bg:${cfg.badge_bg};--fg:${cfg.badge_text};--rec:${cfg.badge_rec_color};--replay:${cfg.badge_replay_color};--alpha:${cfg.badge_opacity}%`
      : "",
  );

  onMount(() => {
    api.status().then((s) => (status = s));
    api.configGet().then((c) => (cfg = c));
    const loadIcon = () => api.badgeIconData().then((d) => (iconSrc = d)).catch(() => (iconSrc = null));
    loadIcon();
    const offs = [
      onClipFlash((secs) => triggerFlash({ kind: "clip", secs })),
      onScreenshotFlash(() => triggerFlash({ kind: "screenshot" })),
      onStatus((s) => (status = s)), onSettings((c) => (cfg = c)), onBadgeIcon(loadIcon),
    ];
    return () => {
      clearTimeout(flashTimer);
      offs.forEach((o) => o.then((f) => f()));
    };
  });

  // Tell the shell the content size whenever it changes (text, size setting, timer on or off).
  $effect(() => {
    if (!pill) return;
    let frame = 0;
    let last = "";
    const report = () => {
      frame = 0;
      const w = Math.ceil(pill!.offsetWidth);
      const h = Math.ceil(pill!.offsetHeight);
      const key = `${w}x${h}`;
      if (w > 0 && key !== last) {
        last = key;
        api.badgeResize(w, h).catch(() => {});
      }
    };
    const ro = new ResizeObserver(() => {
      if (!frame) frame = requestAnimationFrame(report);
    });
    ro.observe(pill);
    return () => {
      ro.disconnect();
      cancelAnimationFrame(frame);
    };
  });
</script>

{#if cfg}
  <div class="badge" class:rec={recording} class:saved={flashing} bind:this={pill} {style} role="status" aria-label={recording ? "Recording" : "Replay on"}>
    {#if showIcon || fallbackIcon}
      <span class="icon"><BadgeIcon icon={fallbackIcon || !cfg.badge_icon ? "dot" : cfg.badge_icon} src={iconSrc} /></span>
    {/if}
    {#if showLabel}<span class="label">{label}</span>{/if}
    {#if showTime}<span class="time">{time}</span>{/if}
  </div>
{/if}

<style>
  :global(html.badge-window),
  :global(html.badge-window body) {
    background: transparent;
    overflow: hidden;
  }
  /* Everything is in em, so the size setting scales the whole pill; it is as wide as its content. */
  .badge {
    --fs: calc(12px * var(--s, 1));
    box-sizing: border-box;
    display: inline-flex;
    align-items: center;
    gap: 0.66em;
    width: max-content;
    height: 2.8em;
    padding: 0 1em;
    border-radius: 999px;
    background: color-mix(in srgb, var(--bg, #121317) var(--alpha, 88%), transparent);
    box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--fg, #f2f2f4) 14%, transparent);
    color: var(--fg, #f2f2f4);
    font: 600 var(--fs) var(--font);
    white-space: nowrap;
    user-select: none;
    animation: in 220ms var(--ease) both;
  }
  .icon {
    display: inline-flex;
    font-size: 1.05em;
    color: var(--replay, #3ba7ff);
  }
  .rec .icon {
    color: var(--rec, #ef4444);
    animation: pulse 1.6s ease-in-out infinite;
  }
  .saved .icon {
    color: #30d158;
    animation: none;
  }
  .label {
    letter-spacing: 0.08em;
  }
  .time {
    color: color-mix(in srgb, var(--fg, #f2f2f4) 68%, transparent);
    font-variant-numeric: tabular-nums;
  }
  @keyframes pulse {
    50% {
      opacity: 0.35;
    }
  }
  @keyframes in {
    from {
      opacity: 0;
      transform: scale(0.94);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .rec .icon {
      animation: none;
    }
  }
</style>
