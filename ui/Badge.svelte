<script lang="ts">
  // The recording badge (src-tauri/src/badge.rs): a small pill in a monitor corner, styled by the
  // user's badge settings. The shell excludes this window from capture and makes it
  // click-through; this page reports its own size so the window is exactly as wide as the text.
  import { onMount } from "svelte";
  import {
    api,
    onBadgeFlash,
    onBadgeIcon,
    onClipFlash,
    onScreenshotFlash,
    onSettings,
    onStatus,
    type BadgeFlash,
    type Settings,
    type Status,
  } from "./api";
  import BadgeIcon from "./BadgeIcon.svelte";

  let status = $state<Status | null>(null);
  let cfg = $state<Settings | null>(null);
  let pill = $state<HTMLDivElement>();
  let iconSrc = $state<string | null>(null);

  const recording = $derived(Boolean(status?.recording));
  const paused = $derived(Boolean(status?.recording && status?.recording_paused));
  const waiting = $derived(status?.phase === "armed" && Boolean(status?.waiting_for_source) && !recording);
  const clock = (t: number) => {
    const s = Math.max(0, Math.floor(t));
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const rem = s % 60;
    return h > 0
      ? `${h}:${String(m).padStart(2, "0")}:${String(rem).padStart(2, "0")}`
      : `${m}:${String(rem).padStart(2, "0")}`;
  };
  const flashDuration = (secs: number) =>
    secs >= 60 ? clock(secs) : `${Math.max(1, Math.round(secs))} s`;

  // Lifecycle notifications (started, paused, resumed, saved, stopped, failed) flash briefly.
  let flash = $state<BadgeFlash | null>(null);
  let flashTimer: ReturnType<typeof setTimeout> | undefined;
  let lastBadgeFlashAt = 0;
  function triggerFlash(next: BadgeFlash) {
    flash = next;
    clearTimeout(flashTimer);
    flashTimer = setTimeout(() => (flash = null), Math.max(800, next.duration_ms || 3500));
  }
  const flashing = $derived(flash !== null && Boolean(cfg?.badge_clip_flash));
  const activeRecClock = $derived(
    clock(status?.mode === "continuous" ? (status?.segment_secs ?? 0) : (status?.recording_secs ?? 0)),
  );
  const time = $derived(
    flashing
      ? flash?.secs != null
        ? flashDuration(flash.secs)
        : recording
          ? activeRecClock
          : ""
      : paused || recording
        ? activeRecClock
        : waiting
          ? ""
          : `${Math.round(status?.buffered_secs ?? 0)} s`,
  );
  const label = $derived(
    flashing
      ? (flash?.label ?? "")
      : paused
        ? "PAUSED"
        : waiting
          ? status?.capture_target?.exe
            ? `WAITING · ${status.capture_target.exe.replace(/\.exe$/i, "")}`
            : "WAITING"
          : ((recording ? cfg?.badge_label_rec : cfg?.badge_label_replay) ?? ""),
  );
  const showLabel = $derived(flashing || paused || waiting || Boolean(cfg?.badge_show_label && label.trim()));
  const showTime = $derived(
    flashing
      ? flash?.secs != null
        ? true
        : Boolean(cfg?.badge_show_time && time)
      : waiting
        ? false
        : Boolean(cfg?.badge_show_time && time),
  );
  const showIcon = $derived(cfg?.badge_icon !== "none");
  /** With everything switched off the badge would be empty, so the dot stays. */
  const fallbackIcon = $derived(!showIcon && !showLabel && !showTime);
  const specialState = $derived(flashing || paused || waiting);
  const tone = $derived(
    flashing
      ? (flash?.tone ?? "ok")
      : paused || waiting
        ? "paused"
        : recording
          ? "rec"
          : "replay",
  );
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
      onBadgeFlash((next) => {
        lastBadgeFlashAt = Date.now();
        triggerFlash(next);
      }),
      onClipFlash((secs) => {
        if (Date.now() - lastBadgeFlashAt > 200) {
          triggerFlash({ kind: "clip_saved", label: "CLIP SAVED", secs, tone: "ok", duration_ms: 3500 });
        }
      }),
      onScreenshotFlash(() => {
        if (Date.now() - lastBadgeFlashAt > 200) {
          triggerFlash({ kind: "screenshot_saved", label: "SCREENSHOT SAVED", secs: null, tone: "ok", duration_ms: 3500 });
        }
      }),
      onStatus((s) => (status = s)),
      onSettings((c) => (cfg = c)),
      onBadgeIcon(loadIcon),
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
  <div
    class="badge"
    class:rec={tone === "rec"}
    class:paused={tone === "paused"}
    class:waiting={!flashing && waiting}
    class:saved={tone === "ok"}
    class:err={tone === "err"}
    bind:this={pill}
    {style}
    role="status"
    aria-label={flashing ? label : paused ? "Paused" : recording ? "Recording" : waiting ? "Waiting for source" : "Replay on"}
  >
    {#if showIcon || fallbackIcon || specialState}
      <span class="icon">
        {#if tone === "ok"}
          <svg viewBox="0 0 24 24" width="1em" height="1em" aria-hidden="true">
            <path d="M5.5 12.5l4.2 4.2L18.5 7.8" fill="none" stroke="currentColor" stroke-width="3" stroke-linecap="round" stroke-linejoin="round" />
          </svg>
        {:else if !flashing && waiting}
          <svg viewBox="0 0 24 24" width="1em" height="1em" aria-hidden="true">
            <circle cx="12" cy="12" r="8.5" fill="none" stroke="currentColor" stroke-width="2.4" />
            <path d="M12 7.5v4.8l3.2 2" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round" />
          </svg>
        {:else if tone === "paused"}
          <svg viewBox="0 0 24 24" width="1em" height="1em" aria-hidden="true">
            <rect x="6" y="5" width="4" height="14" rx="1.4" fill="currentColor" />
            <rect x="14" y="5" width="4" height="14" rx="1.4" fill="currentColor" />
          </svg>
        {:else if tone === "err"}
          <svg viewBox="0 0 24 24" width="1em" height="1em" aria-hidden="true">
            <circle cx="12" cy="12" r="9" fill="none" stroke="currentColor" stroke-width="2.4" />
            <path d="M12 7.5v5.5" fill="none" stroke="currentColor" stroke-width="2.6" stroke-linecap="round" />
            <circle cx="12" cy="16.3" r="1.4" fill="currentColor" />
          </svg>
        {:else}
          <BadgeIcon icon={fallbackIcon || !cfg.badge_icon || cfg.badge_icon === "none" ? "dot" : cfg.badge_icon} src={iconSrc} />
        {/if}
      </span>
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
  .paused .icon {
    color: #f59e0b;
    animation: none;
  }
  .waiting .icon {
    color: #f59e0b;
    animation: pulse 1.8s ease-in-out infinite;
  }
  .saved .icon {
    color: #30d158;
    animation: pop 280ms var(--ease) both;
  }
  .err .icon {
    color: #ef4444;
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
  @keyframes pop {
    from {
      opacity: 0.4;
      transform: scale(0.75);
    }
  }
  @keyframes in {
    from {
      opacity: 0;
      transform: scale(0.94);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .rec .icon,
    .waiting .icon,
    .saved .icon {
      animation: none;
    }
  }
</style>
