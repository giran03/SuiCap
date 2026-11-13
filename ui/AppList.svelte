<script lang="ts">
  // Edit a list of program names (game.exe). Enter or comma adds, × removes.
  let {
    items,
    placeholder,
    onchange,
  }: { items: string[]; placeholder: string; onchange: (v: string[]) => void } = $props();
  let text = $state("");
  let error = $state("");

  function add() {
    let name = text.trim().replace(/^.*[\\/]/, "");
    if (!name) return;
    if (!/\.exe$/i.test(name)) name += ".exe";
    if (!/^[^<>:"/\\|?*\x00-\x1f]{1,60}\.exe$/i.test(name)) {
      error = "Use a program name like game.exe";
      return;
    }
    if (!items.some((i) => i.toLowerCase() === name.toLowerCase())) onchange([...items, name]);
    text = "";
    error = "";
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "Enter" || e.key === ",") {
      e.preventDefault();
      add();
    } else if (e.key === "Backspace" && !text && items.length) {
      onchange(items.slice(0, -1));
    }
  }
</script>

<div class="list">
  {#each items as item (item)}
    <span class="chip">
      {item}
      <button aria-label="Remove {item}" onclick={() => onchange(items.filter((i) => i !== item))}>×</button>
    </span>
  {/each}
  <input
    bind:value={text}
    onkeydown={onKey}
    onblur={add}
    placeholder={items.length ? "" : placeholder}
    aria-label={placeholder}
    spellcheck="false"
  />
</div>
{#if error}<p class="err">{error}</p>{/if}

<style>
  .list {
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
    min-height: 34px;
    padding: 4px;
    border: 1px solid var(--line-strong);
    border-radius: var(--radius-sm);
    background: var(--surface-2);
  }
  .list:focus-within {
    border-color: var(--focus);
  }
  .chip {
    display: inline-flex;
    align-items: center;
    gap: 2px;
    padding: 1px 2px 1px 8px;
    border-radius: 4px;
    background: var(--surface-3);
    font-size: 12.5px;
  }
  .chip button {
    width: 20px;
    height: 20px;
    border: 0;
    border-radius: 3px;
    background: transparent;
    color: var(--text-2);
    cursor: pointer;
  }
  .chip button:hover {
    background: var(--line-strong);
    color: var(--text);
  }
  input {
    flex: 1;
    min-width: 120px;
    border: 0;
    outline: 0;
    background: transparent;
    padding: 2px 4px;
    user-select: text;
  }
  .err {
    margin: 4px 0 0;
    color: var(--warn);
    font-size: 12px;
  }
</style>
