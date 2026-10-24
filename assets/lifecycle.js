// Tracks only whether a page has unsaved input; values never leave the page.
(() => {
  if (window.__asideLifecycle) return;
  const state = { dirty: false };
  window.__asideLifecycle = state;
  addEventListener('input', () => { state.dirty = true; }, true);
  addEventListener('change', () => { state.dirty = true; }, true);
  state.canSuspend = () => !state.dirty && !window.onbeforeunload &&
    ![...document.querySelectorAll('input,textarea')].some(e => !['button','submit','hidden','checkbox','radio'].includes(e.type) && e.value !== e.defaultValue) &&
    ![...document.querySelectorAll('audio,video')].some(e => !e.paused && !e.ended) &&
    navigator.mediaSession?.playbackState !== 'playing';
})();
