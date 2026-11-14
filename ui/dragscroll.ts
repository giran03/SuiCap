// Drag-to-scroll (grab the background and pull), with a little momentum on release. It ignores
// anything interactive, so buttons, sliders, inputs, draggable chips and selectable text keep
// working as usual.

const SKIP =
  "button, input, textarea, select, a, label, [draggable='true'], [role='slider'], [role='listbox'], [role='option'], [contenteditable], .changelog, [data-no-dragscroll]";

export function dragscroll(node: HTMLElement) {
  let id = -1;
  let startY = 0;
  let startX = 0;
  let top0 = 0;
  let left0 = 0;
  let moved = false;
  let vy = 0;
  let vx = 0;
  let lastT = 0;
  let lastY = 0;
  let lastX = 0;
  let raf = 0;

  function down(e: PointerEvent) {
    if (e.button !== 0 || e.pointerType === "touch" || (e.target as HTMLElement).closest(SKIP)) return;
    cancelAnimationFrame(raf);
    id = e.pointerId;
    startY = lastY = e.clientY;
    startX = lastX = e.clientX;
    top0 = node.scrollTop;
    left0 = node.scrollLeft;
    lastT = e.timeStamp;
    moved = false;
    vy = vx = 0;
  }

  function move(e: PointerEvent) {
    if (e.pointerId !== id) return;
    const dy = e.clientY - startY;
    const dx = e.clientX - startX;
    if (!moved) {
      if (Math.hypot(dx, dy) < 5) return;
      moved = true;
      node.setPointerCapture(id);
      node.classList.add("grabbing");
      getSelection()?.removeAllRanges();
    }
    const dt = Math.max(1, e.timeStamp - lastT);
    vy = (lastY - e.clientY) / dt;
    vx = (lastX - e.clientX) / dt;
    lastT = e.timeStamp;
    lastY = e.clientY;
    lastX = e.clientX;
    node.scrollTop = top0 - dy;
    node.scrollLeft = left0 - dx;
  }

  function glide() {
    vy *= 0.94;
    vx *= 0.94;
    node.scrollTop += vy * 16;
    node.scrollLeft += vx * 16;
    if (Math.abs(vy) > 0.02 || Math.abs(vx) > 0.02) raf = requestAnimationFrame(glide);
  }

  function up(e: PointerEvent) {
    if (e.pointerId !== id) return;
    id = -1;
    if (!moved) return;
    node.classList.remove("grabbing");
    if (node.hasPointerCapture(e.pointerId)) node.releasePointerCapture(e.pointerId);
    // a flick that ended in a pause should not coast
    if (e.timeStamp - lastT < 80) raf = requestAnimationFrame(glide);
  }

  /** A drag is not a click: swallow the one that follows. */
  function click(e: MouseEvent) {
    if (moved) {
      e.stopPropagation();
      e.preventDefault();
      moved = false;
    }
  }

  node.addEventListener("pointerdown", down);
  node.addEventListener("pointermove", move);
  node.addEventListener("pointerup", up);
  node.addEventListener("pointercancel", up);
  node.addEventListener("click", click, true);
  return {
    destroy() {
      cancelAnimationFrame(raf);
      node.removeEventListener("pointerdown", down);
      node.removeEventListener("pointermove", move);
      node.removeEventListener("pointerup", up);
      node.removeEventListener("pointercancel", up);
      node.removeEventListener("click", click, true);
    },
  };
}
