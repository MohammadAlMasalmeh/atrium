// Tracks only whether a page has unsaved input; values never leave the page.
// Runs in an isolated world, where the page's `onbeforeunload` is not visible;
// the host checks that separately.
(() => {
  if (window.__atriumLifecycle) return;
  const state = { dirty: false };
  window.__atriumLifecycle = state;
  addEventListener('input', () => { state.dirty = true; }, true);
  addEventListener('change', () => { state.dirty = true; }, true);
  state.canSuspend = () => !state.dirty &&
    ![...document.querySelectorAll('input,textarea')].some(e => !['button','submit','hidden','checkbox','radio'].includes(e.type) && e.value !== e.defaultValue) &&
    ![...document.querySelectorAll('audio,video')].some(e => !e.paused && !e.ended) &&
    navigator.mediaSession?.playbackState !== 'playing';
})();
