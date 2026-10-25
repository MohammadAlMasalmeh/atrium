// Reports video or audio that this Chromium build can't play (it has no H.264, AAC or
// HLS), so the host can offer the page in WebKit. The renderer helper evaluates this in
// every frame's main world before page scripts run, and calls it with a native `report`.
// Nothing is left on the page's globals.
(report) => {
  if (!/^https?:$/.test(location.protocol)) return;
  let sent = false;
  const send = (src) => {
    if (sent) return;
    sent = true;
    report(String(src || location.href).slice(0, 300));
  };
  const PROPRIETARY = /avc[13]|mp4a|hev1|hvc1|mpegurl/i;
  // Players ask before they try: hls.js needs MediaSource with H.264 or native HLS, and
  // shows its own error when neither is there, without the media element ever failing.
  const refused = new Set();
  let timer = null;
  const note = (type, answer) => {
    if (answer || !PROPRIETARY.test(String(type))) return;
    refused.add(/mpegurl/i.test(type) ? 'hls' : 'mse');
    timer ??= setTimeout(settle, 3000);
  };
  // Sites probe many formats and play whichever works (YouTube falls back to VP9), so a
  // refusal only counts if nothing in the frame plays afterwards.
  function settle() {
    const media = [...document.querySelectorAll('video, audio')];
    if (media.some((m) => m.readyState >= 2 || m.currentTime > 0)) return;
    if (media.length || (refused.has('hls') && refused.has('mse'))) send(media[0]?.currentSrc);
  }
  const MS = window.MediaSource;
  if (MS && MS.isTypeSupported) {
    const real = MS.isTypeSupported;
    MS.isTypeSupported = function isTypeSupported(type) { const a = real.call(this, type); note(type, a); return a; };
  }
  const realCan = HTMLMediaElement.prototype.canPlayType;
  HTMLMediaElement.prototype.canPlayType = function canPlayType(type) { const a = realCan.call(this, type); note(type, a); return a; };

  // The container a source is in, from its type or else its file extension.
  const kind = (url, type) => type
    || (/\.(mp4|m4v|mov)([?#]|$)/i.test(url) ? 'video/mp4'
      : /\.(m4a|aac)([?#]|$)/i.test(url) ? 'audio/mp4'
      : /\.m3u8([?#]|$)/i.test(url) ? 'application/vnd.apple.mpegurl' : '');
  function check(m) {
    if (!(m instanceof HTMLMediaElement)) return;
    if (m.error?.code === MediaError.MEDIA_ERR_SRC_NOT_SUPPORTED) return send(m.currentSrc || m.src);
    // MediaSource players (blob: URLs) are judged by what they asked for, above.
    const sources = m.hasAttribute('src') ? [[m.src, '']] : [...m.querySelectorAll('source')].map((s) => [s.src, s.type]);
    if (!sources.length || sources.some(([url]) => url.startsWith('blob:'))) return;
    if (sources.every(([url, type]) => { const k = kind(url, type); return k && !realCan.call(m, k); })) send(sources[0][0]);
  }
  // Media events don't bubble, so listen in the capture phase.
  for (const type of ['error', 'loadstart', 'play']) {
    addEventListener(type, (e) => {
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
  }).observe(document, { childList: true, subtree: true });
}
