// Reports video or audio that this Chromium build can't play (it has no H.264, AAC or
// HLS), so the host can offer the page in WebKit. Evaluated with passwords.js in the
// same private isolated world of the top frame, and sent through the same binding.
(() => {
  if (window !== window.top || window.__atriumMedia) return;
  window.__atriumMedia = true;
  let sent = false;
  const report = (src) => {
    if (sent) return;
    sent = true;
    try { window.__atriumPasswords(JSON.stringify({ type: 'media', origin: location.origin, src: String(src).slice(0, 300) })); } catch {}
  };
  // The container a source is in, from its type or else its file extension.
  const kind = (url, type) => type
    || (/\.(mp4|m4v|mov)([?#]|$)/i.test(url) ? 'video/mp4'
      : /\.(m4a|aac)([?#]|$)/i.test(url) ? 'audio/mp4'
      : /\.m3u8([?#]|$)/i.test(url) ? 'application/vnd.apple.mpegurl' : '');
  function check(m) {
    if (!(m instanceof HTMLMediaElement)) return;
    if (m.error?.code === MediaError.MEDIA_ERR_SRC_NOT_SUPPORTED) return report(m.currentSrc || m.src);
    // MediaSource players (blob: URLs) choose formats themselves; only plain sources are judged.
    const sources = m.hasAttribute('src') ? [[m.src, '']] : [...m.querySelectorAll('source')].map((s) => [s.src, s.type]);
    if (!sources.length || sources.some(([url]) => url.startsWith('blob:'))) return;
    // Chromium can't play any source: every one is a format it says it doesn't support.
    if (sources.every(([url, type]) => { const k = kind(url, type); return k && !m.canPlayType(k); })) report(sources[0][0]);
  }
  // Media events don't bubble, so listen in the capture phase.
  for (const type of ['error', 'loadstart', 'play']) {
    document.addEventListener(type, (e) => {
      const t = e.target;
      // A failed <source> moves on to the next one first; check once that settles.
      setTimeout(() => check(t instanceof HTMLSourceElement ? t.parentElement : t), 0);
    }, true);
  }
  // Players often add their element after load.
  new MutationObserver((records) => {
    if (sent) return;
    for (const r of records) for (const n of r.addedNodes) {
      if (n instanceof HTMLSourceElement) check(n.parentElement);
      else if (n instanceof Element) (n.matches('video, audio') ? [n] : n.querySelectorAll('video, audio')).forEach(check);
    }
  }).observe(document.documentElement, { childList: true, subtree: true });
  document.querySelectorAll('video, audio').forEach(check);
})();
