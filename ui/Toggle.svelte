<script lang="ts">
  // SuiNotch-style switch: spring knob that stretches while pressed.
  import { showMenu } from "./menu-state.svelte";
  let { on, label, onchange }: { on: boolean; label: string; onchange: (v: boolean) => void } = $props();
</script>

<button
  class="switch"
  class:on
  role="switch"
  aria-checked={on}
  aria-label={label}
  onclick={() => onchange(!on)}
  oncontextmenu={(e) => showMenu(e, [{ header: label }, { label: on ? "Turn off" : "Turn on", run: () => onchange(!on) }])}
>
  <span></span>
</button>

<style>
  .switch {
    position: relative;
    flex: none;
    width: 44px;
    height: 24px;
    padding: 0;
    border: 0;
    border-radius: 12px;
    background: var(--off);
    cursor: pointer;
    transition: background var(--med) var(--ease);
  }
  .switch:hover {
    background: var(--off-hover);
  }
  .switch.on,
  .switch.on:hover {
    background: var(--accent);
  }
  span {
    position: absolute;
    top: 3px;
    left: 3px;
    width: 18px;
    height: 18px;
    border-radius: 9px;
    background: var(--knob);
    box-shadow: 0 1px 3px rgb(0 0 0 / 0.3);
    transition:
      transform var(--slow) var(--spring),
      width var(--fast) var(--ease);
  }
  .on span {
    transform: translateX(20px);
  }
  .switch:active span {
    width: 22px;
  }
  .on:active span {
    transform: translateX(16px);
  }
</style>
