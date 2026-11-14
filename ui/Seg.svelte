<script lang="ts" generics="T extends string | number | null">
  // Segmented control with a thumb that springs between options (SuiNotch "Size" row).
  let {
    options,
    value,
    onselect,
    label,
  }: {
    options: { value: T; label: string; disabled?: boolean; hint?: string }[];
    value: T;
    onselect: (v: T) => void;
    label?: string;
  } = $props();

  let root: HTMLDivElement;
  let thumb = $state({ x: 0, w: 0, ready: false });

  const index = $derived(options.findIndex((o) => o.value === value));

  function measure() {
    const b = root?.querySelectorAll<HTMLButtonElement>("button")[index];
    if (b) thumb = { x: b.offsetLeft, w: b.offsetWidth, ready: true };
  }

  $effect(() => {
    void index;
    void options.length;
    measure();
  });
</script>

<svelte:window onresize={measure} />

<div class="seg" bind:this={root} role="radiogroup" aria-label={label}>
  {#if index >= 0}
    <span class="thumb" class:ready={thumb.ready} style="transform: translateX({thumb.x}px); width: {thumb.w}px"></span>
  {/if}
  {#each options as o (o.label)}
    <button
      role="radio"
      aria-checked={o.value === value}
      aria-disabled={o.disabled || undefined}
      class:current={o.value === value}
      class:off={o.disabled}
      title={o.hint}
      onclick={() => !o.disabled && onselect(o.value)}
    >
      {o.label}
    </button>
  {/each}
</div>

<style>
  .seg {
    position: relative;
    display: inline-flex;
    flex: none;
    padding: 3px;
    border-radius: var(--radius-md);
    background: var(--surface-2);
  }
  .thumb {
    position: absolute;
    top: 3px;
    bottom: 3px;
    left: 0;
    border-radius: 8px;
    background: var(--surface-3);
    box-shadow: 0 1px 4px rgb(0 0 0 / 0.18);
  }
  .thumb.ready {
    transition:
      transform var(--slow) var(--spring),
      width var(--med) var(--ease);
  }
  button {
    position: relative;
    min-width: 54px;
    height: 28px;
    padding: 0 12px;
    border: 0;
    border-radius: 8px;
    background: transparent;
    color: var(--text-3);
    font-size: 12.5px;
    font-weight: 550;
    cursor: pointer;
    transition: color var(--fast) var(--ease);
  }
  button:hover {
    color: var(--text-2);
  }
  button.current {
    color: var(--text);
  }
  button.off {
    color: var(--text-3);
    opacity: 0.4;
    cursor: not-allowed;
  }
  button.off:hover {
    color: var(--text-3);
  }
  button.off:active {
    transform: none;
  }
  button:active:not(.off) {
    transform: scale(0.97);
  }
</style>
