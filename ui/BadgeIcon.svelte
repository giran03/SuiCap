<script lang="ts" module>
  export type BadgeIconName = "dot" | "ring" | "square" | "play" | "camera" | "bolt" | "custom" | "none";
  export const BADGE_ICONS: { id: BadgeIconName; label: string }[] = [
    { id: "dot", label: "Dot" },
    { id: "ring", label: "Ring" },
    { id: "square", label: "Square" },
    { id: "play", label: "Play" },
    { id: "camera", label: "Camera" },
    { id: "bolt", label: "Bolt" },
    { id: "custom", label: "Your image" },
    { id: "none", label: "No icon" },
  ];
</script>

<script lang="ts">
  // The badge glyph, drawn in currentColor so the page colors it (recording or replay color).
  // `src` is the user's own image (a data URL) for the "custom" icon; without one it shows the dot.
  let { icon, src = null }: { icon: BadgeIconName; src?: string | null } = $props();
</script>

{#if icon === "custom" && src}
  <img {src} alt="" draggable="false" style="width: 1.2em; height: 1.2em; object-fit: contain; border-radius: 0.2em" />
{:else if icon !== "none"}
  <svg viewBox="0 0 24 24" width="1em" height="1em" aria-hidden="true">
    {#if icon === "dot" || icon === "custom"}
      <circle cx="12" cy="12" r="8" fill="currentColor" />
    {:else if icon === "ring"}
      <circle cx="12" cy="12" r="6.5" fill="none" stroke="currentColor" stroke-width="3.2" />
    {:else if icon === "square"}
      <rect x="5.5" y="5.5" width="13" height="13" rx="2.8" fill="currentColor" />
    {:else if icon === "play"}
      <path d="M7.5 4.8v14.4l12-7.2z" fill="currentColor" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round" />
    {:else if icon === "camera"}
      <rect x="2.5" y="6.5" width="13" height="11" rx="2.8" fill="currentColor" />
      <path d="M16.5 10.6l5-3v8.8l-5-3z" fill="currentColor" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" />
    {:else if icon === "bolt"}
      <path d="M13.2 2.5L5 13.4h5.8l-1 8.1 8.2-11h-5.8z" fill="currentColor" stroke="currentColor" stroke-width="1" stroke-linejoin="round" />
    {/if}
  </svg>
{/if}
