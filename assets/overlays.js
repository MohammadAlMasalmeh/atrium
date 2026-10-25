// Removes the invisible layers popunder scripts stretch over a whole page, so the first
// click anywhere opens an ad instead of reaching what was clicked. The domain list can't
// keep up with these scripts: they are served from the site's own CDN and fall back to
// fresh domains when theirs are blocked. The renderer helper evaluates this in every
// frame's main world before page scripts run, unless content blocking is off. Nothing is
// left on the page's globals.
(() => {
  if (!/^https?:$/.test(location.protocol)) return;
  // Popunder layers sit above everything; site UI stays far below this.
  const ABOVE = 1e6;
  const REPLACED = new Set(['IFRAME', 'IMG', 'VIDEO', 'CANVAS', 'OBJECT', 'EMBED', 'svg']);
  const style = getComputedStyle;
  function clickjacks(el) {
    if (!(el instanceof Element) || !el.isConnected) return false;
    const s = style(el);
    if (s.pointerEvents === 'none' || (s.position !== 'fixed' && s.position !== 'absolute') || !(+s.zIndex >= ABOVE)) return false;
    const r = el.getBoundingClientRect(), root = document.documentElement;
    if (r.width < root.clientWidth * 0.9 || r.height < root.clientHeight * 0.9) return false;
    if (+s.opacity < 0.1) return true;
    // Shows nothing, yet takes every click.
    return !REPLACED.has(el.tagName) && !el.childElementCount && !el.textContent.trim()
      && s.backgroundImage === 'none' && /^(transparent|rgba\(.*, 0\))$/.test(s.backgroundColor);
  }
  const removed = new WeakSet();
  const drop = (el) => { removed.add(el); el.remove(); };

  // They are added to <body> or <html>, so only those are watched, which stays cheap.
  const watched = new WeakSet();
  const observer = new MutationObserver((records) => {
    for (const r of records) for (const n of r.addedNodes) visit(n);
  });
  // The parser may have added <body> by the time <html> is seen, so children are visited too.
  function visit(node) {
    if (node !== document && node !== document.documentElement && node !== document.body) {
      if (clickjacks(node)) drop(node);
      return;
    }
    if (watched.has(node)) return;
    watched.add(node);
    observer.observe(node, { childList: true });
    for (const child of node.children) visit(child);
  }
  visit(document);

  // One made elsewhere, or shown after it was added, is caught when it takes a click.
  // The rest of that click's events still go to it, so they are stopped too.
  for (const type of ['pointerdown', 'mousedown', 'pointerup', 'mouseup', 'click', 'touchstart', 'touchend', 'auxclick']) {
    addEventListener(type, (e) => {
      const t = e.target;
      if (!removed.has(t) && !clickjacks(t)) return;
      if (t.isConnected) drop(t);
      e.preventDefault();
      e.stopImmediatePropagation();
    }, true);
  }
})();
