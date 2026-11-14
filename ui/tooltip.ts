// One shared tooltip bubble for the whole page, ported from the CafeTako Site Builder
// (apps/editor/src/ui/Tooltip.tsx, "Tooltip open/close"). Any element with `data-tooltip="..."`
// or a native `title` gets it. The native title is held back only while the bubble is open and
// then restored. Moving between neighbouring triggers tweens the single bubble instead of
// popping a new one; CSS owns every tween (`.t-tt` in theme.css).

const SELECTOR = "[data-tooltip],[title]:not(iframe)";
const GAP = 8;
const MARGIN = 8;
const LEAVE_DELAY = 150;

export function mountTooltips(): () => void {
  const id = `t-tt-${Math.random().toString(36).slice(2, 8)}`;
  const tip = document.createElement("span");
  tip.id = id;
  tip.className = "t-tt";
  tip.setAttribute("role", "tooltip");
  tip.setAttribute("aria-hidden", "true");
  tip.dataset.show = "false";
  const text = document.createElement("span");
  text.className = "t-tt-text";
  tip.appendChild(text);
  document.body.appendChild(tip);

  let current: Element | null = null;
  let nativeTitle: string | null = null;
  let hovered: Element | null = null;
  let focused: Element | null = null;
  let overTip = false;
  let dismissed: Element | null = null;
  let leaveTimer: number | undefined;

  const cancelLeave = () => window.clearTimeout(leaveTimer);

  const release = () => {
    if (!current) return;
    // Remove only our description; the control may have others.
    const rest = (current.getAttribute("aria-describedby") ?? "").split(/\s+/).filter((v) => v && v !== id);
    if (rest.length) current.setAttribute("aria-describedby", rest.join(" "));
    else current.removeAttribute("aria-describedby");
    if (nativeTitle !== null && !current.hasAttribute("title")) current.setAttribute("title", nativeTitle);
    current = null;
    nativeTitle = null;
  };

  const hide = () => {
    cancelLeave();
    release();
    tip.dataset.show = "false";
    tip.setAttribute("aria-hidden", "true");
  };

  const place = (target: Element) => {
    cancelLeave();
    const title = target === current ? nativeTitle : target.getAttribute("title");
    const label = target.getAttribute("data-tooltip") ?? title ?? "";
    if (!label) return hide();
    const showing = tip.dataset.show === "true";
    if (target !== current) {
      release();
      current = target;
      nativeTitle = target.getAttribute("title");
      // Keep a title-derived name when the trigger has no other accessible label.
      if (target.hasAttribute("aria-label") || target.hasAttribute("aria-labelledby") || target.textContent?.trim())
        target.removeAttribute("title");
    }
    const described = (target.getAttribute("aria-describedby") ?? "").split(/\s+/).filter(Boolean);
    if (!described.includes(id)) target.setAttribute("aria-describedby", [...described, id].join(" "));
    text.textContent = label;

    // Measure the natural size (CSS caps it), then tween from the previous width.
    const previousWidth = tip.style.width;
    tip.style.transition = "none";
    tip.style.width = "max-content";
    const width = tip.offsetWidth + 1;
    const height = tip.offsetHeight;
    tip.style.width = previousWidth;
    void tip.offsetWidth;
    if (showing) tip.style.transition = "";

    const r = target.getBoundingClientRect();
    const x = Math.min(Math.max(MARGIN, r.left + r.width / 2 - width / 2), Math.max(MARGIN, innerWidth - width - MARGIN));
    const above = r.top - height - GAP >= MARGIN;
    const y = Math.max(MARGIN, Math.min(above ? r.top - height - GAP : r.bottom + GAP, innerHeight - height - MARGIN));
    tip.style.transformOrigin = above ? "50% 100%" : "50% 0";
    tip.style.width = `${width}px`;
    tip.style.setProperty("--tt-x", `${x}px`);
    tip.style.setProperty("--tt-y", `${y}px`);
    if (!showing) {
      // Snap the geometry while hidden so only the appear plays.
      void tip.offsetWidth;
      tip.style.transition = "";
    }
    tip.dataset.show = "true";
    tip.setAttribute("aria-hidden", "false");
  };

  const targetOf = (t: EventTarget | null): Element | null => {
    if (!(t instanceof Element)) return null;
    const match = t.closest(SELECTOR);
    // An active title has been suppressed; descendants still belong to its trigger.
    return current?.contains(t) && (!match || match.contains(current)) ? current : match;
  };

  const update = () => {
    cancelLeave();
    if (overTip) return;
    const target = hovered ?? focused;
    if (target && target !== dismissed) {
      if (target !== current) place(target);
    } else {
      // Allow crossing the small gap between the trigger and its bubble.
      leaveTimer = window.setTimeout(hide, LEAVE_DELAY);
    }
  };

  const onOver = (e: PointerEvent) => {
    if (e.pointerType === "touch") return;
    overTip = e.target instanceof Node && tip.contains(e.target);
    if (!overTip) {
      hovered = targetOf(e.target);
      if (hovered !== dismissed) dismissed = null;
    }
    update();
  };
  const onFocus = (e: FocusEvent) => {
    focused = e.target instanceof Element && e.target.matches(":focus-visible") ? targetOf(e.target) : null;
    if (focused) {
      hovered = null;
      overTip = false;
    }
    dismissed = null;
    update();
  };
  const onBlur = () => {
    focused = null;
    update();
  };
  const onGone = () => {
    dismissed = current;
    hovered = focused = null;
    overTip = false;
    hide();
  };
  const onLeave = (e: PointerEvent) => {
    if (e.pointerType === "touch") return;
    overTip = e.relatedTarget instanceof Node && tip.contains(e.relatedTarget);
    if (!overTip) hovered = targetOf(e.relatedTarget);
    if (hovered !== dismissed) dismissed = null;
    update();
  };
  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") onGone();
  };

  document.addEventListener("pointerover", onOver);
  document.addEventListener("pointerout", onLeave);
  document.addEventListener("focusin", onFocus);
  document.addEventListener("focusout", onBlur);
  document.addEventListener("pointerdown", onGone, true);
  document.addEventListener("scroll", onGone, true);
  document.addEventListener("keydown", onKey);
  window.addEventListener("blur", onGone);
  window.addEventListener("resize", onGone);

  return () => {
    document.removeEventListener("pointerover", onOver);
    document.removeEventListener("pointerout", onLeave);
    document.removeEventListener("focusin", onFocus);
    document.removeEventListener("focusout", onBlur);
    document.removeEventListener("pointerdown", onGone, true);
    document.removeEventListener("scroll", onGone, true);
    document.removeEventListener("keydown", onKey);
    window.removeEventListener("blur", onGone);
    window.removeEventListener("resize", onGone);
    hide();
    tip.remove();
  };
}
