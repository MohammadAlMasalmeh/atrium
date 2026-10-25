// First-run setup: Welcome → Import from Chrome → Set up the assistant.
// Runs on top of the shell as the 'onboarding' overlay, so the shell's own
// render() hides native page views while it is open.
(() => {
const { S, store, render, post } = window.__shell;
const $ = (s, r = document) => r.querySelector(s);
const esc = (s) => String(s ?? '').replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
const svg = (d, cls = '') => `<svg class="${cls}" viewBox="0 0 24 24">${d}</svg>`;
const G = {
  check: '<path d="m5 12.5 4.5 4.5L19 7.5"/>',
  x: '<path d="M16 8 8 16M8 8l8 8"/>',
  back: '<path d="m14.5 6-6 6 6 6"/>',
  updown: '<path d="m8 9 4-4 4 4M8 15l4 4 4-4"/>',
  page: '<path d="M14 3.5H7A2.5 2.5 0 0 0 4.5 6v12A2.5 2.5 0 0 0 7 20.5h10a2.5 2.5 0 0 0 2.5-2.5V9Z"/><path d="M14 3.5V9h5.5M8.5 13h7M8.5 16.5h4.5"/>',
  cursor: '<path d="M6 4.5 11.8 19l2-6 6.2-2Z"/><path d="m14 14 4.5 4.5"/>',
  sidebar: '<rect x="3" y="4.5" width="18" height="15" rx="3"/><path d="M9.5 4.5v15M5.5 8.5h1.5M5.5 11.5h1.5"/>',
  arrow: '<path d="M8 16 16 8M9.5 8H16v6.5"/>',
  agent: '<rect x="3" y="4.5" width="18" height="15" rx="3"/><path d="m7.5 10 3 2.5-3 2.5M13 15.5h3.5"/>',
};
const CHROME = `<svg viewBox="0 0 24 24"><path fill="#EA4335" d="M12 6h10.39A12 12 0 0 0 1.61 6L6.8 15A6 6 0 0 1 12 6Z"/><path fill="#FBBC04" d="M17.2 15 12 24A12 12 0 0 0 22.39 6H12a6 6 0 0 1 5.2 9Z"/><path fill="#34A853" d="M6.8 15 1.61 6A12 12 0 0 0 12 24l5.2-9a6 6 0 0 1-10.4 0Z"/><circle cx="12" cy="12" r="6" fill="#fff"/><circle cx="12" cy="12" r="4.7" fill="#4285F4"/></svg>`;
const MARK = '<svg viewBox="10 10 80 82"><path fill-rule="evenodd" d="M10 92V50a40 40 0 0 1 80 0v42ZM36 92V64a14 14 0 0 1 28 0v28Z"/></svg>';
const INSTALL_CMD = 'curl -fsSL https://opencode.ai/install | bash';

const STEPS = ['welcome', 'import', 'assistant'];
const saved = store.get('onboarding', {});
const st = {
  step: Math.min(saved.step || 0, STEPS.length - 1),
  profiles: null, profile: null, opts: { bookmarks: true, history: true },
  imported: saved.imported || null, importing: false, importError: '',
  oc: null, ocTask: null, ocError: '', ocFailed: null, serverOpen: false,
  pct: null, holding: false, celebrate: true, introduced: false, listenUntil: 0,
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const reduceMotion = matchMedia('(prefers-reduced-motion: reduce)').matches;
const save = () => store.set('onboarding', { step: st.step, imported: st.imported, done: !!saved.done });

const root = document.createElement('div');
root.id = 'ob';
root.setAttribute('role', 'dialog'); root.setAttribute('aria-modal', 'true'); root.setAttribute('aria-labelledby', 'ob-title');
root.innerHTML = `<div class="ob-top" data-drag><button class="ob-back" id="ob-back">${svg(G.back)}Back</button>
    <div class="ob-dots" aria-hidden="true">${STEPS.map(() => '<i></i>').join('')}</div></div>
  <div class="ob-stage"></div>
  <footer class="ob-foot"><button class="ob-go" id="ob-go"></button><button class="ob-skip" id="ob-skip"></button></footer>`;
document.body.appendChild(root);
const stage = $('.ob-stage', root);
const title = (t, sub) => `<h1 class="ob-title" id="ob-title">${t}</h1>${sub ? `<p class="ob-sub">${sub}</p>` : ''}`;

/* ── Screens ─────────────────────────────────────── */
// Each returns { html, primary: [label, handler, {busy, disabled}], skip: label|null, bind? }.
const screens = {
  welcome: () => ({
    html: `<div class="ob-hero app">${MARK}</div>` + title('Welcome to Atrium') + `<div class="ob-body"><div class="ob-features">
      <div class="ob-feature">${svg(G.page)}<div><b>Ask about any page</b><p>Get summaries and answers from the pages you have open.</p></div></div>
      <div class="ob-feature">${svg(G.cursor)}<div><b>Let the assistant do the clicking</b><p>It can search, fill in forms and gather details in its own tab while you keep browsing.</p></div></div>
      <div class="ob-feature">${svg(G.sidebar)}<div><b>Keep tabs organized</b><p>Group tabs into spaces in the sidebar. Tabs you haven’t used in a while sleep to save memory.</p></div></div>
    </div></div>`,
    primary: ['Continue', next], skip: null,
  }),

  import: () => {
    const hero = `<div class="ob-hero">${CHROME}</div>`;
    if (st.profiles === null) {
      loadProfiles();
      return { html: hero + title('Import from Chrome', 'Bring your bookmarks and history to Atrium.'), primary: ['Continue', next, { disabled: true }], skip: null };
    }
    if (st.imported) {
      const r = st.imported;
      const parts = [r.bookmarks && `${r.bookmarks.toLocaleString()} bookmarks`, r.history && `${r.history.toLocaleString()} history items`].filter(Boolean);
      return {
        html: hero + title('Import Complete', parts.length ? `Atrium imported ${parts.join(' and ')} from Chrome.` : 'That profile didn’t have any bookmarks or history to import.')
          + (r.warnings?.length ? `<div class="ob-body"><div class="ob-note">${r.warnings.map(esc).join('<br>')}</div></div>` : ''),
        primary: ['Continue', next], skip: null,
      };
    }
    if (!st.profiles.length) return {
      html: hero + title('Import from Chrome', 'Chrome isn’t set up on this Mac, so there’s nothing to import.'),
      primary: ['Continue', next], skip: null,
    };
    const many = st.profiles.length > 1;
    const sw = (k, t, d) => `<div class="ob-row"><div class="txt"><b>${t}</b><span>${d}</span></div>
      <button class="ob-switch" role="switch" aria-checked="${st.opts[k]}" data-k="${k}" aria-label="${t}" ${st.importing ? 'disabled' : ''}></button></div>`;
    return {
      html: hero + title('Import from Chrome', 'Bring your bookmarks and history to Atrium. Passwords and cookies stay in Chrome.') + `<div class="ob-body">
        <div class="ob-group">
          ${many ? `<div class="ob-row"><div class="txt"><b>Profile</b></div><label class="ob-popup"><span>${esc(st.profiles.find((p) => p.id === st.profile)?.name)}</span>${svg(G.updown)}<select id="ob-profile">${st.profiles.map((p) => `<option value="${esc(p.id)}" ${p.id === st.profile ? 'selected' : ''}>${esc(p.name)}</option>`).join('')}</select></label></div>` : ''}
          ${sw('bookmarks', 'Bookmarks', 'Your bookmarks bar becomes Favorites')}
          ${sw('history', 'History', 'The last 400 sites you visited')}
        </div>
        ${st.importError ? `<div class="ob-note err">${esc(st.importError)}</div>` : ''}</div>`,
      primary: [st.importing ? 'Importing…' : 'Import', runImport, { busy: st.importing, disabled: !st.opts.bookmarks && !st.opts.history }],
      skip: 'Not Now',
      bind: () => {
        root.querySelectorAll('.ob-switch').forEach((b) => b.onclick = () => { st.opts[b.dataset.k] = !st.opts[b.dataset.k]; draw(); });
        const sel = $('#ob-profile'); if (sel) sel.onchange = () => { st.profile = sel.value; draw(); };
      },
    };
  },

  assistant: () => {
    const oc = st.oc;
    const ready = oc?.healthy && oc?.browser_tools;
    if (ready && !st.holding) return tryIt();
    const t = st.ocTask;
    const hero = `<div class="ob-hero tile">${svg(G.agent, 'sym')}</div>`;
    const head = hero + title('Set Up the Assistant', 'Atrium’s assistant runs on OpenCode, an open-source AI agent on your Mac. Free models are included, so there’s nothing to sign up for.');
    if (!oc) { refreshOc(); return { html: head, primary: ['Continue', finish, { disabled: true }], skip: null }; }
    const local = /^http:\/\/(127\.0\.0\.1|localhost)[:/]/.test(oc.endpoint || '');
    // Show what's already on this Mac before doing anything with it.
    if (oc.installed && local && !st.introduced) {
      st.introduced = true;
      termPush('cmd', 'which opencode'); termPush('out', oc.path || 'opencode');
      if (oc.version) { termPush('cmd', 'opencode --version'); termPush('out', oc.version); }
    }
    // Do the work we can do without asking: if OpenCode is installed, start it now.
    if (oc.installed && !oc.healthy && !st.ocTask && !st.ocError && !st.autoStarted && local) { st.autoStarted = true; setTimeout(setupOc, 600); }
    // Only a step that just finished gets the check animation, not every redraw.
    const marks = st.marks || (st.marks = {});
    const mark = (s, k) => s === 'done' ? `<span class="ob-mark done${marks[k] && marks[k] !== 'done' ? ' just' : ''}">${svg(G.check)}</span>`
      : s === 'busy' ? `<span class="ob-mark"><span class="spin"></span></span>`
      : s === 'fail' ? `<span class="ob-mark fail">${svg(G.x)}</span>` : `<span class="ob-mark todo"></span>`;
    const failed = (k) => st.ocError && st.ocFailed === k ? 'fail' : 'todo';
    const installing = t === 'install';
    const rows = [
      ['Install OpenCode', installing ? `<span id="ob-install-sub">${installStatus()}</span><span class="ob-bar${st.pct == null ? ' indet' : ''}"><i id="ob-bar" style="width:${st.pct ?? 0}%"></i></span>`
        : oc.installed ? (oc.version ? `Version ${esc(oc.version)}` : 'Installed') : !local && oc.healthy ? 'Using a server on another machine' : 'About 40 MB from opencode.ai',
        oc.installed || (!local && oc.healthy) ? 'done' : installing ? 'busy' : failed('install')],
      ['Start OpenCode', oc.healthy ? `Running at ${esc((oc.endpoint || '').replace(/^https?:\/\//, ''))}` : 'Runs in the background while Atrium is open',
        oc.healthy ? 'done' : t === 'start' ? 'busy' : failed('start')],
      ['Connect to your tabs', oc.browser_tools ? 'The assistant can read and use your tabs' : 'So the assistant can read pages and click for you',
        oc.browser_tools ? 'done' : t === 'start' && oc.healthy ? 'busy' : failed('tools')],
    ];
    const label = t === 'install' ? 'Installing…' : t ? 'Starting…' : ready ? 'All Set' : st.ocError ? 'Try Again' : !oc.installed && local ? 'Install OpenCode' : 'Start OpenCode';
    const server = st.serverOpen ? `<div class="ob-group"><div class="ob-fields">
        <input id="ob-url" placeholder="http://127.0.0.1:4096" value="${esc(local ? '' : oc.endpoint || '')}" spellcheck="false" aria-label="Server address">
        <input id="ob-pass" type="password" placeholder="Password (optional)" aria-label="Server password">
        <div class="row2"><button class="ob-small" id="ob-server-cancel">Cancel</button><button class="ob-small primary" id="ob-connect">Connect</button></div></div></div>` : '';
    let note = '';
    if (st.ocError) {
      note = `<div class="ob-note err">${esc(st.ocError)}</div>`;
      if (st.ocFailed === 'install') note += `<div class="ob-note">You can also install it in Terminal, then click Try Again.</div><div class="ob-code"><code>${INSTALL_CMD}</code><button id="ob-copy">Copy</button></div>`;
    }
    const alt = server || (t || ready ? '' : `<div class="ob-note"><button class="ob-link" id="ob-server">Connect to an existing OpenCode server…</button></div>`);
    return {
      html: head + `<div class="ob-body">
        <div class="ob-group">${rows.map(([b, s, m]) => {
          const html = `<div class="ob-row${m === 'todo' ? ' dim' : ''}">${mark(m, b)}<div class="txt"><b>${b}</b><span>${s}</span></div></div>`;
          marks[b] = m; return html;
        }).join('')}</div>
        ${(local || term.lines.length) && !st.serverOpen ? termHtml() : ''}
        ${note}${alt}</div>`,
      primary: [label, setupOc, { busy: !!t, disabled: ready }],
      skip: t || ready ? null : 'Set Up Later',
      bind: () => {
        const b = $('#ob-server'); if (b) b.onclick = () => { st.serverOpen = true; draw(); setTimeout(() => $('#ob-url')?.focus(), 30); };
        const c = $('#ob-server-cancel'); if (c) c.onclick = () => { st.serverOpen = false; draw(); };
        const k = $('#ob-connect'); if (k) k.onclick = connectServer;
        const cp = $('#ob-copy'); if (cp) cp.onclick = () => { navigator.clipboard.writeText(INSTALL_CMD); cp.textContent = 'Copied'; };
      },
    };
  },
};

// Once the assistant works, the best introduction is using it: send a first
// request from here and land in the chat while it runs.
const TRY = [
  ['whats on the front page of hacker news rn', 'Reads a site and sums it up'],
  ['find me a good ramen place near me thats open late', 'Searches, compares and links its sources'],
  ['what are people saying about the new iphone on reddit', 'Gathers opinions across several pages'],
];
function tryIt() {
  const model = S.models.find((m) => m.id === S.model)?.name;
  return {
    html: `<div class="ob-hero tile${st.celebrate && !reduceMotion ? ' pop' : ''}">${svg(G.agent, 'sym')}<span class="ob-badge">${svg(G.check)}</span></div>`
      + title('Your Assistant Is Ready', 'Ask about a page, or have it do something on the web for you. Try it now:')
      + `<div class="ob-body">
        <form class="ob-ask" id="ob-ask"><input id="ob-ask-input" placeholder="Ask anything" autocomplete="off" spellcheck="false" aria-label="Ask the assistant">
          <button type="submit" aria-label="Send">${svg('<path d="M12 19V5M5.5 11.5 12 5l6.5 6.5"/>')}</button></form>
        <div class="ob-group">${TRY.map(([q, d], i) => `<button class="ob-row ob-try" data-i="${i}"><div class="txt"><b>${esc(q)}</b><span>${d}</span></div>${svg(G.arrow, 'go')}</button>`).join('')}</div>
        ${model ? `<div class="ob-note">Using ${esc(model.split(' · ').pop())}. To use your own model provider, run <code>opencode auth login</code> in Terminal.</div>` : ''}
      </div>`,
    key: 'ready', primary: ['Start Browsing', finish], skip: null,
    bind: () => {
      st.celebrate = false;
      const send = (q) => { if (!q.trim()) return; finish(); window.__shell.askAgent(q.trim()); };
      $('#ob-ask').onsubmit = (e) => { e.preventDefault(); send($('#ob-ask-input').value); };
      root.querySelectorAll('.ob-try').forEach((b) => b.onclick = () => send(TRY[b.dataset.i][0]));
    },
  };
}

/* ── Terminal ────────────────────────────────────── */
// Mirrors what setup runs: the installer's and server's real output (streamed
// as atrium.setup.* events), with each command typed out as it starts.
const term = { lines: [], queue: [], running: false };
const installStatus = () => st.pct == null ? 'Fetching the installer…' : st.pct < 100 ? `Downloading… ${Math.floor(st.pct)}%` : 'Unpacking…';
// SGR colours the OpenCode installer uses: 2 muted, 31 red, 38;5;214 orange.
function ansi(s) {
  let cls = '', html = '';
  for (const part of String(s).split(/(\x1b\[[0-9;?]*[A-Za-z])/)) {
    if (part.startsWith('\x1b')) {
      const c = /^\x1b\[([0-9;]*)m$/.exec(part)?.[1];
      if (c != null) cls = /^0?$/.test(c) ? '' : /(^|;)2$/.test(c) ? 'd' : /(^|;)31$/.test(c) ? 'r' : /38;5;/.test(c) ? 'o' : cls;
    } else if (part) html += cls ? `<span class="${cls}">${esc(part)}</span>` : esc(part);
  }
  return html;
}
function progBar() {
  const p = st.pct ?? 0, n = Math.round(p / 100 * 28);
  return `<span class="o">${'█'.repeat(n)}</span><span class="d">${'░'.repeat(28 - n)}</span> ${String(Math.floor(p)).padStart(3, '\u00a0')}%`;
}
const lineHtml = (l) => l.kind === 'cmd' ? `<span class="ps">$</span> ${esc(l.text)}`
  : l.kind === 'ok' ? `<span class="g">✓</span> ${esc(l.text)}`
  : l.kind === 'err' ? `<span class="r">✗ ${esc(l.text)}</span>`
  : l.kind === 'info' ? `<span class="d">${esc(l.text)}</span>`
  : l.kind === 'prog' ? progBar() : ansi(l.text);
const caretHtml = () => st.ocTask || term.running ? '<span class="caret"></span>'
  : `<span class="ps">$</span> ${!st.oc?.installed && !term.lines.length ? `<span class="d">${INSTALL_CMD}</span>` : ''}<span class="caret"></span>`;
const termHtml = () => `<div class="ob-term"><div class="ob-term-bar"><i></i><i></i><i></i><span>opencode — setup</span></div>
  <div class="ob-term-body" id="ob-term">${term.lines.map((l) => `<div class="${lnClass(l)}">${lineHtml(l)}</div>`).join('')}<div class="ln">${caretHtml()}</div></div></div>`;
const toBottom = (el) => { el.scrollTop = el.scrollHeight; };
// Block-character art (the installer's logo) needs rows that touch.
const lnClass = (l) => 'ln ' + l.kind + (/[▀▄█░]/.test(l.text) && l.kind === 'out' ? ' art' : '');

function termPush(kind, text = '') {
  term.queue.push({ kind, text });
  // Start on the next tick so the screen that asked for it has been drawn.
  if (!term.running) { term.running = true; setTimeout(termPump); }
}
async function termPump() {
  while (term.queue.length) {
    const l = term.queue.shift();
    term.lines.push(l);
    if (term.lines.length > 400) term.lines.splice(0, term.lines.length - 400);
    const body = $('#ob-term', root);
    if (!body) continue;
    const el = document.createElement('div');
    el.className = lnClass(l);
    body.insertBefore(el, body.lastElementChild);
    body.lastElementChild.innerHTML = caretHtml();
    const hurry = reduceMotion || term.queue.length > 24;
    if (l.kind === 'cmd' && !hurry) {
      el.innerHTML = '<span class="ps">$</span> ';
      const typed = el.appendChild(document.createTextNode(''));
      const step = Math.max(1, Math.ceil(l.text.length / 36));
      for (let i = step; i < l.text.length + step; i += step) { typed.data = l.text.slice(0, i); toBottom(body); await sleep(18); }
      await sleep(140);
    } else {
      el.innerHTML = lineHtml(l);
      toBottom(body);
      if (!hurry) await sleep(l.kind === 'out' ? 14 : 90);
    }
  }
  term.running = false;
  const body = $('#ob-term', root);
  if (body) { body.lastElementChild.innerHTML = caretHtml(); toBottom(body); }
}
function termProgress(pct) {
  st.pct = pct;
  if (!term.lines.some((l) => l.kind === 'prog') && !term.queue.some((l) => l.kind === 'prog')) termPush('prog');
  const bars = root.querySelectorAll('#ob-term .ln.prog');
  if (bars.length) bars[bars.length - 1].innerHTML = progBar();
  const bar = $('#ob-bar', root);
  if (bar) { bar.style.width = pct + '%'; bar.parentElement.classList.remove('indet'); }
  const sub = $('#ob-install-sub', root); if (sub) sub.textContent = installStatus();
}
function onSetupEvent(ev) {
  // Events can trail the HTTP response that ends a task by a moment.
  if (S.overlay !== 'onboarding' || (!st.ocTask && Date.now() > st.listenUntil)) return;
  if (ev.type === 'atrium.setup.progress') termProgress(ev.pct);
  else termPush(ev.kind || 'out', ev.text ?? '');
}

/* ── Actions ─────────────────────────────────────── */
async function loadProfiles() {
  if (st.loadingProfiles) return; st.loadingProfiles = true;
  try { const r = await (await fetch('/api/setup/profiles')).json(); st.profiles = r.profiles || []; }
  catch { st.profiles = []; }
  st.profile = st.profiles[0]?.id || null; draw();
}
async function runImport() {
  st.importing = true; st.importError = ''; draw();
  const r = await post('/api/setup/import', { profile: st.profile, ...st.opts, extensions: false });
  st.importing = false;
  if (r.error) { st.importError = r.error; draw(); return; }
  if (r.history?.length) {
    const seen = new Set(S.hist.map((h) => h.url));
    S.hist = [...S.hist, ...r.history.filter((h) => !seen.has(h.url))].sort((a, b) => b.t - a.t).slice(0, 400);
    store.set('history', S.hist);
  }
  if (r.bookmarks?.length) {
    store.set('bookmarks', r.bookmarks);
    const bar = r.bookmarks.filter((b) => /bookmark/i.test(b.folder) || !b.folder);
    const hosts = new Set(), favs = [];
    for (const b of (bar.length ? bar : r.bookmarks)) {
      const h = new URL(b.url).hostname; if (!hosts.has(h)) { hosts.add(h); favs.push(b.url); }
      if (favs.length === 8) break;
    }
    if (favs.length) { S.favs = favs; store.set('favs', favs); }
  }
  st.imported = { bookmarks: r.bookmarks?.length || 0, history: r.history?.length || 0, warnings: r.warnings || [] };
  save(); render(); draw();
}
async function refreshOc() {
  try { st.oc = await (await fetch('/api/setup/opencode')).json(); } catch { st.oc = { installed: false, healthy: false, browser_tools: false }; }
  if (STEPS[st.step] === 'assistant') draw();
}
function endTask(error, failed) {
  st.ocTask = null; st.listenUntil = Date.now() + 1500;
  if (error) { st.ocError = error; st.ocFailed = failed; st.holding = false; termPush('err', error); }
}
async function setupOc() {
  st.ocError = ''; st.ocFailed = null; st.serverOpen = false; st.holding = true; st.pct = null;
  if (!st.oc.installed) {
    st.ocTask = 'install'; draw();
    const r = await post('/api/setup/opencode/install', {});
    if (r.error) { endTask(r.error, 'install'); await refreshOc(); return; }
    await refreshOc();
  }
  st.ocTask = 'start'; draw();
  const poll = setInterval(refreshOc, 1000);
  const r = await post('/api/setup/opencode/start', {});
  clearInterval(poll);
  endTask(r.error, st.oc?.healthy ? 'tools' : 'start');
  await refreshOc();
  if (r.error) return;
  await window.__shell.refreshModels();
  // Let the checklist and terminal finish before switching to the ready screen.
  await sleep(300);
  while (term.running) await sleep(50);
  await sleep(900);
  st.holding = false; st.celebrate = true; draw();
}
async function connectServer() {
  const url = $('#ob-url').value.trim(), password = $('#ob-pass').value;
  if (!url) { $('#ob-url').focus(); return; }
  st.ocTask = 'start'; st.ocError = ''; st.holding = false; draw();
  const r = await post('/api/setup/opencode/connect', { url, password });
  endTask(r.error, 'start');
  if (r.error) st.serverOpen = true;
  else { st.serverOpen = false; st.celebrate = true; await window.__shell.refreshModels(); }
  await refreshOc();
}

/* ── Navigation ──────────────────────────────────── */
let current, shown;
function go(i) { st.step = Math.max(0, Math.min(STEPS.length - 1, i)); save(); draw(); }
function next() { go(st.step + 1); }
function finish() { saved.done = true; save(); S.overlay = null; render(); }
function draw() {
  if (S.overlay !== 'onboarding') return;
  const name = STEPS[st.step];
  current = screens[name]();
  const key = current.key || name;
  // Animate in only when the screen changes, not on every state update.
  const step = document.createElement('div');
  step.className = 'ob-step ' + name + (shown === key ? '' : ' fresh');
  step.innerHTML = current.html;
  const focused = document.activeElement?.id;
  // Keep the live terminal (its scroll position and in-progress typing) across redraws.
  const oldTerm = $('#ob-term', stage), newTerm = $('#ob-term', step);
  if (oldTerm && newTerm) { newTerm.replaceWith(oldTerm); oldTerm.lastElementChild.innerHTML = caretHtml(); }
  stage.replaceChildren(step);
  if (newTerm) toBottom($('#ob-term', step));
  shown = key;
  root.querySelectorAll('.ob-dots i').forEach((d, i) => d.classList.toggle('on', i === st.step));
  current.bind?.();
  if (focused && $('#' + focused, root)) $('#' + focused, root).focus();
  const [label, , o = {}] = current.primary;
  const goBtn = $('#ob-go');
  goBtn.innerHTML = o.busy ? `<span class="spin"></span>${esc(label)}` : esc(label);
  goBtn.disabled = !!o.busy || !!o.disabled;
  $('#ob-back').hidden = st.step === 0 || !!o.busy;
  const skip = $('#ob-skip');
  skip.textContent = current.skip || ''; skip.hidden = !current.skip;
}
$('#ob-go').onclick = () => { const [, fn, o = {}] = current.primary; if (!o.busy && !o.disabled) fn(); };
$('#ob-back').onclick = () => go(st.step - 1);
$('#ob-skip').onclick = () => st.step === STEPS.length - 1 ? finish() : next();
document.addEventListener('keydown', (e) => {
  if (S.overlay !== 'onboarding') return;
  if (e.key === 'Escape') { e.preventDefault(); e.stopImmediatePropagation(); finish(); return; }
  if (e.key !== 'Enter' || e.metaKey || e.isComposing) return;
  if (e.target.matches?.('#ob-url, #ob-pass')) { e.preventDefault(); connectServer(); return; }
  if (e.target.id === 'ob-ask-input') return;
  if (e.target.closest?.('#ob button, select')) return;   // focused controls handle Return themselves
  e.preventDefault(); $('#ob-go').click();
}, true);

function open(step) {
  st.step = step != null ? Math.max(0, STEPS.indexOf(step)) : saved.done ? 0 : st.step;
  st.oc = null; st.autoStarted = false; st.ocError = ''; shown = null; S.overlay = 'onboarding';
  st.introduced = false; st.celebrate = true; st.holding = false; st.pct = null; st.marks = {};
  term.lines = []; term.queue = [];
  render(); draw();
  setTimeout(() => $('#ob-go').focus({ preventScroll: true }), 50);
}
window.atriumOnboarding = { open, close: finish, onSetupEvent, get visible() { return S.overlay === 'onboarding'; } };
if (!saved.done) open();
})();
