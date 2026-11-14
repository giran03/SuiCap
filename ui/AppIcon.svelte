<script lang="ts">
  // An app's icon, or a colored letter tile while it loads or when none can be found.
  import { cachedIcon, iconFor } from "./icons";

  let { exe, size = 22 }: { exe: string; size?: number } = $props();
  let url = $state<string | null>(null);

  $effect(() => {
    const e = exe;
    url = cachedIcon(e);
    if (!url) iconFor(e).then((u) => e === exe && (url = u));
  });

  // Stable hue per program so the fallback tiles are told apart.
  const hue = $derived([...exe].reduce((h, c) => (h * 31 + c.charCodeAt(0)) % 360, 7));
</script>

{#if url}
  <img src={url} alt="" width={size} height={size} draggable="false" />
{:else}
  <span class="tile" style="width: {size}px; height: {size}px; background: hsl({hue} 55% 42%); font-size: {size * 0.5}px">
    {exe.charAt(0).toUpperCase()}
  </span>
{/if}

<style>
  img {
    flex: none;
    object-fit: contain;
    animation: fade var(--med) var(--ease);
  }
  .tile {
    display: grid;
    flex: none;
    place-items: center;
    border-radius: 6px;
    color: #fff;
    font-weight: 700;
  }
  @keyframes fade {
    from {
      opacity: 0;
      transform: scale(0.85);
    }
  }
</style>
