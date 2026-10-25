// Password form detection, evaluated by the host in a private isolated world of the top
// frame. Page scripts share the DOM but cannot see this world's globals or the host
// binding. Saved passwords arrive only through fill(), which the host calls for this exact
// origin after the user chooses to fill, and only reach the chosen field.
(() => {
  if (window !== window.top) return;
  if (window.__atriumPw) { window.__atriumPw.report(true); return; }
  const BINDING = '__atriumPasswords';
  const send = (msg) => { try { window[BINDING](JSON.stringify(msg)); } catch {} };
  const valueSetter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
  const TEXT = new Set(['text', 'email', 'tel', 'url']);
  const ac = (el) => (el.getAttribute('autocomplete') || '').toLowerCase();

  function usable(el) {
    if (!el.isConnected || el.disabled || el.readOnly) return false;
    const s = getComputedStyle(el), r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0 && s.visibility !== 'hidden' && s.display !== 'none';
  }

  function usernameFor(scope, first) {
    const inputs = [...scope.querySelectorAll('input')].filter((i) => TEXT.has(i.type) && usable(i));
    const tagged = inputs.find((i) => /\b(username|email)\b/.test(ac(i)));
    if (tagged) return tagged;
    const before = inputs.filter((i) => i.compareDocumentPosition(first) & Node.DOCUMENT_POSITION_FOLLOWING);
    return before[before.length - 1] || null;
  }

  // One group per form (or the formless page), classified as login, signup, or change.
  function groups() {
    const byScope = new Map();
    for (const p of document.querySelectorAll('input[type=password]')) {
      if (!usable(p)) continue;
      const scope = p.form || document.body;
      (byScope.get(scope) || byScope.set(scope, []).get(scope)).push(p);
    }
    return [...byScope].map(([scope, fields]) => {
      const fresh = fields.filter((f) => ac(f).includes('new-password'));
      const current = fields.filter((f) => ac(f).includes('current-password'));
      const words = `${scope.getAttribute('action') || ''} ${scope.id} ${scope.className} ${(scope.innerText || '').slice(0, 2000)}`.toLowerCase();
      let kind = 'login';
      if (fresh.length && current.length || fields.length >= 3) kind = 'change';
      else if (fresh.length || fields.length === 2) kind = 'signup';
      else if (!current.length && /sign ?up|register|create (an |your )?account/.test(words) && !/sign ?in|log ?in/.test(words)) kind = 'signup';
      const newFields = kind === 'login' ? [] : fresh.length ? fresh : kind === 'change' ? fields.slice(1) : fields;
      const password = kind === 'login' ? fields[0] : current[0] || (kind === 'change' ? fields[0] : null);
      return { scope, kind, fields, newFields, password, user: usernameFor(scope, fields[0]) };
    });
  }

  let reported = '';
  function report(force) {
    const all = groups();
    const msg = { type: 'forms', origin: location.origin, login: all.some((g) => g.kind === 'login'), signup: all.some((g) => g.kind !== 'login') };
    const key = JSON.stringify(msg);
    if (force || key !== reported) { reported = key; send(msg); }
  }
  let timer = 0;
  new MutationObserver(() => { clearTimeout(timer); timer = setTimeout(report, 250); })
    .observe(document.documentElement, { subtree: true, childList: true, attributes: true, attributeFilter: ['type', 'style', 'class', 'hidden', 'disabled'] });

  let last = { key: '', at: 0 };
  function capture(target) {
    const g = groups().find((x) => x.scope === target || x.scope.contains(target));
    if (!g) return;
    const fields = g.kind === 'login' ? [g.password] : g.newFields;
    const password = fields[0]?.value || '';
    if (!password || fields.some((f) => f.value !== password)) return;
    const username = (g.user?.value || '').trim();
    const key = JSON.stringify([username, password]);
    if (key === last.key && Date.now() - last.at < 3000) return;
    last = { key, at: Date.now() };
    send({ type: 'submit', origin: location.origin, kind: g.kind, username, password });
  }
  const SUBMIT_WORDS = /log ?in|sign ?in|sign ?up|register|create|continue|next|submit|save|join/i;
  window.addEventListener('submit', (e) => capture(e.target), true);
  window.addEventListener('click', (e) => {
    const b = e.target.closest?.('button, input[type=submit], input[type=button], [role=button]');
    if (!b || b.type === 'reset') return;
    if (b.form && b.type === 'submit') return; // The submit event follows.
    if (SUBMIT_WORDS.test(b.innerText || b.value || b.getAttribute('aria-label') || '')) capture(b.form || b);
  }, true);
  window.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && e.target.type === 'password' && !e.target.form) capture(e.target);
  }, true);

  function set(el, value) {
    el.focus({ preventScroll: true });
    valueSetter.call(el, value);
    el.dispatchEvent(new Event('input', { bubbles: true }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
  }
  function fill({ origin, username, password }) {
    if (location.origin !== origin) return { filled: 0, error: 'origin' };
    const g = groups().find((x) => x.kind === 'login') || groups().find((x) => x.password);
    if (!g?.password) return { filled: 0, error: 'no-form' };
    if (g.user && username) set(g.user, username);
    set(g.password, password);
    return { filled: g.user && username ? 2 : 1 };
  }
  function generate({ origin, password }) {
    if (location.origin !== origin) return { filled: 0, error: 'origin' };
    const g = groups().find((x) => x.newFields.length);
    if (!g) return { filled: 0, error: 'no-form' };
    g.newFields.forEach((f) => set(f, password));
    return { filled: g.newFields.length };
  }
  window.__atriumPw = { fill, generate, report };
  report();
})();
