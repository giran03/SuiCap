<script lang="ts">
  // Slider with a value label. Double-click the label to type an exact value: Enter or blur
  // applies it, Escape cancels. Typed values may go past the slider's range up to hardMin/hardMax.
  import { showMenu, type MenuItem } from "./menu-state.svelte";
  let {
    value,
    min,
    max,
    step = 1,
    hardMin = min,
    hardMax = max,
    label,
    format = (v: number) => String(v),
    unitHint = "",
    parse,
    defaultValue,
    presets = [],
    onchange,
  }: {
    value: number;
    min: number;
    max: number;
    step?: number;
    hardMin?: number;
    hardMax?: number;
    label: string;
    format?: (v: number) => string;
    unitHint?: string;
    parse?: (text: string) => number | null;
    /** The factory value; "Reset to default" appears when the slider is elsewhere. */
    defaultValue?: number;
    /** Quick picks offered in the context menu. */
    presets?: number[];
    onchange: (v: number) => void;
  } = $props();

  let editing = $state(false);
  let text = $state("");
  let box = $state<HTMLInputElement>();

  const pct = $derived(Math.min(100, Math.max(0, ((value - min) / (max - min)) * 100)));

  async function begin() {
    text = String(value);
    editing = true;
    await Promise.resolve();
    box?.focus();
    box?.select();
  }

  function commit() {
    if (!editing) return;
    editing = false;
    const n = parse ? parse(text) : Number(text.trim());
    if (n === null || !Number.isFinite(n)) return;
    const v = Math.min(hardMax, Math.max(hardMin, Math.round(n)));
    if (v !== value) onchange(v);
  }

  function menu(e: MouseEvent) {
    const items: MenuItem[] = [
      { label: "Type an exact value…", run: begin },
      ...(defaultValue !== undefined
        ? [{ label: "Reset to default", hint: format(defaultValue), disabled: value === defaultValue, run: () => onchange(defaultValue) }]
        : []),
    ];
    if (presets.length) {
      items.push({ separator: true }, { header: "Quick picks" });
      for (const p of presets) items.push({ label: format(p), checked: p === value, run: () => onchange(p) });
    }
    showMenu(e, items);
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "Enter") {
      e.preventDefault();
      commit();
    } else if (e.key === "Escape") {
      e.stopPropagation();
      editing = false;
    }
  }
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="range" oncontextmenu={menu}>
  <input
    type="range"
    {min}
    {max}
    {step}
    value={Math.min(max, Math.max(min, value))}
    style="--p: {pct}%"
    aria-label={label}
    oninput={(e) => onchange(+e.currentTarget.value)}
  />
  {#if editing}
    <input
      class="exact"
      bind:this={box}
      bind:value={text}
      inputmode="numeric"
      aria-label="{label}, exact value"
      onkeydown={onKey}
      onblur={commit}
    />
  {:else}
    <output ondblclick={begin} title="Double-click to type an exact value{unitHint ? ` (${unitHint})` : ''}">{format(value)}</output>
  {/if}
</div>

<style>
  .range {
    display: flex;
    align-items: center;
    gap: 14px;
    flex: none;
  }
  output,
  .exact {
    width: 76px;
    height: 28px;
    border-radius: 8px;
    font-size: 12.5px;
    font-variant-numeric: tabular-nums;
    text-align: right;
  }
  output {
    display: inline-flex;
    align-items: center;
    justify-content: flex-end;
    padding: 0 8px;
    color: var(--text-2);
    cursor: text;
    transition: background var(--fast) var(--ease);
  }
  output:hover {
    background: var(--surface-2);
  }
  .exact {
    padding: 0 8px;
    border: 0;
    outline: 0;
    background: var(--accent-soft);
    box-shadow: inset 0 0 0 1.5px var(--accent);
    user-select: text;
    animation: pop var(--med) var(--spring);
  }
  @keyframes pop {
    from {
      transform: scale(0.92);
    }
  }
  input[type="range"] {
    appearance: none;
    width: clamp(110px, 24vw, 220px);
    height: 20px;
    background: transparent;
    cursor: pointer;
  }
  input[type="range"]::-webkit-slider-runnable-track {
    height: 6px;
    border-radius: 3px;
    background: linear-gradient(to right, var(--accent) var(--p), var(--off) var(--p));
  }
  input[type="range"]::-webkit-slider-thumb {
    appearance: none;
    width: 18px;
    height: 18px;
    margin-top: -6px;
    border-radius: 50%;
    background: var(--knob);
    box-shadow: 0 1px 4px rgb(0 0 0 / 0.4);
    transition: transform var(--med) var(--spring);
  }
  input[type="range"]:hover::-webkit-slider-thumb {
    transform: scale(1.12);
  }
  input[type="range"]:active::-webkit-slider-thumb {
    transform: scale(1.25);
  }
</style>
