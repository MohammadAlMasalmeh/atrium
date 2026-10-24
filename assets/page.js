// Injected into every page. Defines window.__aside, the agent's view of and
// hands on the page. Modeled on Aside's agent: a compact accessibility tree
// where actionable elements carry refs (e12) that stay stable while the
// element lives, so successive snapshots can be diffed.
(() => {
  if (window.__aside && window.__aside.v === 5) return;
  const refs = new Map();          // ref -> element
  const refOf = new WeakMap();     // element -> ref (keeps refs stable)
  let counter = 0;

  const clean = (s, n = 100) => (s || '').replace(/\s+/g, ' ').trim().slice(0, n);
  const cut = (s, n = 200) => { const c = (s || '').replace(/\s+/g, ' ').trim(); return c.length > n ? c.slice(0, n) + '…' : c; };

  function hidden(el) {
    const s = getComputedStyle(el);
    if (s.display === 'none' || s.visibility === 'hidden' || s.visibility === 'collapse') return true;
    if (el.getAttribute('aria-hidden') === 'true') return true;
    const r = el.getBoundingClientRect();
    return r.width < 1 && r.height < 1 && s.overflow !== 'visible';
  }

  const ROLES = new Set(['button', 'link', 'textbox', 'searchbox', 'combobox', 'checkbox', 'radio', 'switch', 'slider',
    'spinbutton', 'tab', 'menuitem', 'menuitemcheckbox', 'menuitemradio', 'option', 'treeitem', 'heading', 'img']);
  // Their name says it all, so children are skipped.
  const LEAF = new Set(['button', 'link', 'heading', 'img', 'tab', 'menuitem', 'menuitemcheckbox', 'menuitemradio', 'option',
    'checkbox', 'radio', 'switch', 'slider', 'searchbox', 'combobox', 'file-input', 'textbox']);
  const INTERACTIVE = new Set([...ROLES, 'clickable', 'file-input'].filter((r) => r !== 'heading' && r !== 'img'));
  // Structure worth keeping as a nesting level.
  const CONTAINER_TAGS = { NAV: 'navigation', MAIN: 'main', FORM: 'form', UL: 'list', OL: 'list', TABLE: 'table', TR: 'row',
    ARTICLE: 'article', DIALOG: 'dialog', ASIDE: 'complementary', HEADER: 'banner', FOOTER: 'contentinfo', LI: 'listitem' };
  const CONTAINER_ROLES = new Set(['dialog', 'alertdialog', 'navigation', 'main', 'form', 'search', 'list', 'listbox', 'menu', 'menubar',
    'tablist', 'table', 'grid', 'row', 'article', 'region', 'complementary', 'banner', 'contentinfo', 'listitem', 'tabpanel', 'alert', 'status']);
  const INLINE = new Set(['B', 'I', 'EM', 'STRONG', 'SPAN', 'SMALL', 'CODE', 'ABBR', 'TIME', 'SUP', 'SUB', 'MARK', 'U', 'S', 'Q', 'CITE', 'BR', 'FONT', 'KBD', 'DATA']);

  function roleOf(el) {
    const explicit = el.getAttribute('role');
    if (explicit && ROLES.has(explicit)) return explicit;
    const t = el.tagName;
    if (t === 'A' && el.hasAttribute('href')) return 'link';
    if (t === 'BUTTON' || t === 'SUMMARY') return 'button';
    if (t === 'INPUT') {
      const ty = (el.getAttribute('type') || 'text').toLowerCase();
      if (ty === 'hidden') return null;
      if (['button', 'submit', 'reset', 'image'].includes(ty)) return 'button';
      if (ty === 'checkbox' || ty === 'radio') return ty;
      if (ty === 'range') return 'slider';
      if (ty === 'file') return 'file-input';
      return ty === 'search' ? 'searchbox' : 'textbox';
    }
    if (t === 'TEXTAREA') return 'textbox';
    if (t === 'SELECT') return 'combobox';
    if (/^H[1-6]$/.test(t)) return 'heading';
    if (t === 'IMG' && el.alt && el.alt.trim()) return 'img';
    if (el.isContentEditable && (!el.parentElement || !el.parentElement.isContentEditable)) return 'textbox';
    if (el.hasAttribute('onclick') || (el.tabIndex >= 0 && el.hasAttribute('tabindex') && t !== 'DIV' && t !== 'SECTION')) return 'clickable';
    if (el.hasAttribute('onclick') || el.getAttribute('tabindex') === '0') return 'clickable';
    const s = getComputedStyle(el);
    if (s.cursor === 'pointer' && el.parentElement && getComputedStyle(el.parentElement).cursor !== 'pointer' && !el.querySelector('a,button')) return 'clickable';
    return null;
  }

  function containerOf(el) {
    const r = el.getAttribute('role');
    if (r && CONTAINER_ROLES.has(r)) return r;
    const c = CONTAINER_TAGS[el.tagName];
    if (c === 'banner' || c === 'contentinfo') return el.closest('article, section, main') ? null : c;
    if (el.tagName === 'SECTION' && (el.getAttribute('aria-label') || el.getAttribute('aria-labelledby'))) return 'region';
    return c || null;
  }

  function nameOf(el) {
    let s = el.getAttribute('aria-label') || '';
    if (!s) {
      const ids = el.getAttribute('aria-labelledby');
      if (ids) s = ids.split(/\s+/).map((id) => document.getElementById(id)?.innerText || '').join(' ');
    }
    if (!s && el.labels && el.labels.length) s = el.labels[0].innerText;
    if (!s) s = el.getAttribute('alt') || el.getAttribute('title') || '';
    if (!s && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA')) s = el.getAttribute('placeholder') || '';
    if (!s && el.tagName !== 'INPUT' && el.tagName !== 'SELECT' && el.tagName !== 'TEXTAREA') s = el.innerText || el.textContent || '';
    return clean(s, 120);
  }

  function describe(el, role) {
    const bits = [];
    const t = el.tagName;
    if (role === 'heading') bits.push('level=' + (t[1] || el.getAttribute('aria-level') || ''));
    if (t === 'INPUT' || t === 'TEXTAREA') {
      const ty = (el.getAttribute('type') || '').toLowerCase();
      if (ty === 'checkbox' || ty === 'radio') bits.push(el.checked ? 'checked' : 'unchecked');
      else if (ty === 'password') bits.push(el.value ? 'value=•••' : 'empty');
      else if (el.value) bits.push('value="' + clean(el.value, 80) + '"');
      if (el.placeholder && nameOf(el) !== clean(el.placeholder, 120)) bits.push('placeholder="' + clean(el.placeholder, 50) + '"');
    }
    if (t === 'SELECT') {
      const opt = el.options[el.selectedIndex];
      bits.push('selected="' + clean(opt ? opt.text : '', 40) + '"');
      bits.push('options=[' + Array.from(el.options).slice(0, 15).map((o) => JSON.stringify(clean(o.text, 30))).join(', ') + (el.options.length > 15 ? ', …' : '') + ']');
    }
    if (el.isContentEditable && el.innerText.trim()) bits.push('text="' + clean(el.innerText, 80) + '"');
    if (role === 'link') {
      const href = el.getAttribute('href') || '';
      if (href && !href.startsWith('javascript:') && href !== '#') bits.push('href=' + clean(href, 90));
    }
    if (el.disabled || el.getAttribute('aria-disabled') === 'true') bits.push('disabled');
    if (el.getAttribute('aria-expanded')) bits.push('expanded=' + el.getAttribute('aria-expanded'));
    if (el.getAttribute('aria-pressed')) bits.push('pressed=' + el.getAttribute('aria-pressed'));
    if (el.getAttribute('aria-selected') === 'true' || el.getAttribute('aria-current')) bits.push('selected');
    if (el.required) bits.push('required');
    return bits.join(' ');
  }

  function refFor(el, prefix) {
    let r = refOf.get(el);
    if (!r || refs.get(r) !== el) { r = prefix + 'e' + (++counter); refOf.set(el, r); }
    refs.set(r, el);
    return r;
  }

  /** options: { interactive, scope (ref or CSS selector), maxLines } */
  function snapshot(opts = {}) {
    const interactive = !!opts.interactive;
    const maxLines = opts.maxLines || (interactive ? 500 : 900);
    for (const [r, e] of refs) if (!e.isConnected) refs.delete(r);
    let truncated = false, count = 0;

    // Returns an array of lines for el's subtree at the given depth.
    function walk(root, depth, prefix) {
      const out = [];
      let buf = '';
      const flush = () => { if (buf.trim() && !interactive) out.push('  '.repeat(depth) + '- text "' + cut(buf) + '"'); buf = ''; };
      for (const node of root.childNodes) {
        if (count >= maxLines) { truncated = true; break; }
        if (node.nodeType === 3) { buf += node.textContent; continue; }
        if (node.nodeType !== 1) continue;
        const el = node, tag = el.tagName;
        if (tag === 'SCRIPT' || tag === 'STYLE' || tag === 'NOSCRIPT' || tag === 'TEMPLATE' || tag === 'svg' || tag === 'SVG' || tag === 'LINK' || tag === 'META') continue;
        if (hidden(el) && !el.querySelector('input:focus')) continue;
        const role = roleOf(el);
        if (!role && INLINE.has(tag) && !el.querySelector('a,button,input,select,textarea,[role],[onclick],[tabindex],img')) {
          buf += ' ' + (el.innerText || '') + ' ';
          continue;
        }
        if (role && !(interactive && !INTERACTIVE.has(role))) {
          const name = nameOf(el), extra = describe(el, role);
          if (name || extra || role !== 'clickable') {
            flush();
            out.push('  '.repeat(depth) + `- ${role}${name ? ' "' + name + '"' : ''} [${refFor(el, prefix)}]${extra ? ' ' + extra : ''}`);
            count++;
            if (LEAF.has(role)) continue;
            const inner = walk(el, depth + 1, prefix);
            out.push(...inner);
            continue;
          }
        }
        if (tag === 'IFRAME') {
          try {
            const doc = el.contentDocument;
            if (doc && doc.body) {
              flush();
              out.push('  '.repeat(depth) + '- iframe');
              out.push(...walk(doc.body, depth + 1, 'f' + (++counter)));
            }
          } catch {}
          continue;
        }
        const container = containerOf(el);
        if (container) {
          flush();
          const inner = walk(el, depth + 1, prefix);
          if (inner.length) {
            const label = el.getAttribute('aria-label');
            out.push('  '.repeat(depth) + `- ${container}${label ? ' "' + clean(label, 60) + '"' : ''}`, ...inner);
            count++;
          }
          continue;
        }
        const block = !INLINE.has(tag);
        if (block) flush();
        out.push(...walk(el, depth, prefix));
        if (el.shadowRoot) out.push(...walk(el.shadowRoot, depth, prefix));
        if (block) flush();
      }
      flush();
      return out;
    }

    let root = document.body;
    if (opts.scope) {
      root = refs.get(opts.scope) || document.querySelector(opts.scope);
      if (!root) throw new Error(`Nothing matches scope ${opts.scope}`);
    }
    const lines = root ? walk(root, 0, '') : [];
    const sy = Math.round(scrollY), max = Math.max(0, document.documentElement.scrollHeight - innerHeight);
    const dialog = document.querySelector('dialog[open], [role=dialog]:not([aria-hidden=true]), [aria-modal=true]');
    return {
      url: location.href,
      title: document.title,
      scroll: `${sy}/${Math.round(max)}px`,
      modal: !!(dialog && !hidden(dialog)),
      tree: lines.join('\n') + (truncated ? '\n- … (truncated: scope the snapshot to a section, or use read_page)' : ''),
    };
  }

  // Resolves once the DOM has been quiet for `quiet` ms (or after `max`).
  function settle(quiet = 350, max = 3000) {
    return new Promise((resolve) => {
      let timer;
      const done = () => { obs.disconnect(); clearTimeout(cap); resolve(true); };
      const obs = new MutationObserver(() => { clearTimeout(timer); timer = setTimeout(done, quiet); });
      obs.observe(document.documentElement, { subtree: true, childList: true, attributes: true, characterData: true });
      timer = setTimeout(done, quiet);
      const cap = setTimeout(done, max);
    });
  }

  function el(ref) {
    const e = refs.get(ref);
    if (!e || !e.isConnected) throw new Error(`Element ${ref} is gone. Take a fresh snapshot.`);
    return e;
  }

  const KEYS = {
    enter: ['Enter', 'Enter', 13], tab: ['Tab', 'Tab', 9], escape: ['Escape', 'Escape', 27], esc: ['Escape', 'Escape', 27],
    backspace: ['Backspace', 'Backspace', 8], delete: ['Delete', 'Delete', 46], space: [' ', 'Space', 32],
    arrowup: ['ArrowUp', 'ArrowUp', 38], arrowdown: ['ArrowDown', 'ArrowDown', 40], arrowleft: ['ArrowLeft', 'ArrowLeft', 37],
    arrowright: ['ArrowRight', 'ArrowRight', 39], pageup: ['PageUp', 'PageUp', 33], pagedown: ['PageDown', 'PageDown', 34],
    home: ['Home', 'Home', 36], end: ['End', 'End', 35],
  };

  // Briefly outline what the agent is acting on, so the user can follow along.
  function flash(e) {
    const r = e.getBoundingClientRect();
    const d = document.createElement('div');
    d.style.cssText = `position:fixed;z-index:2147483647;pointer-events:none;left:${r.left - 4}px;top:${r.top - 4}px;` +
      `width:${r.width + 8}px;height:${r.height + 8}px;border-radius:10px;border:2px solid rgba(140,120,255,.95);` +
      'box-shadow:0 0 0 4px rgba(140,120,255,.18),0 0 24px rgba(120,160,255,.55);transition:opacity .5s ease .6s;opacity:1';
    document.documentElement.appendChild(d);
    requestAnimationFrame(() => { d.style.opacity = '0'; });
    setTimeout(() => d.remove(), 1300);
  }

  // Clicks like a user would: pointer + mouse events at the element's center,
  // aimed at whatever is actually on top there if it belongs to the element.
  function click(ref) {
    const e = el(ref);
    e.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
    const r = e.getBoundingClientRect();
    const x = r.left + r.width / 2, y = r.top + r.height / 2;
    const top = document.elementFromPoint(x, y);
    const covered = !!top && !(top === e || e.contains(top) || top.contains(e));
    flash(e);
    const target = top && e.contains(top) ? top : e;
    const opts = { bubbles: true, cancelable: true, composed: true, clientX: x, clientY: y, button: 0, view: window };
    target.dispatchEvent(new PointerEvent('pointerover', opts));
    target.dispatchEvent(new PointerEvent('pointerdown', { ...opts, buttons: 1, pointerType: 'mouse', isPrimary: true }));
    target.dispatchEvent(new MouseEvent('mousedown', { ...opts, buttons: 1 }));
    if (typeof e.focus === 'function') e.focus({ preventScroll: true });
    target.dispatchEvent(new PointerEvent('pointerup', { ...opts, pointerType: 'mouse', isPrimary: true }));
    target.dispatchEvent(new MouseEvent('mouseup', opts));
    target.click();
    return { covered, coveredBy: covered ? clean(top.tagName + ' ' + (top.innerText || ''), 60) : null };
  }

  function key(name, target) {
    const t = target || document.activeElement || document.body;
    const k = KEYS[name.toLowerCase()] || [name, name.length === 1 ? 'Key' + name.toUpperCase() : name, name.toUpperCase().charCodeAt(0)];
    const init = { key: k[0], code: k[1], keyCode: k[2], which: k[2], bubbles: true, cancelable: true, composed: true };
    const down = t.dispatchEvent(new KeyboardEvent('keydown', init));
    if (k[0] === 'Enter') t.dispatchEvent(new KeyboardEvent('keypress', { ...init, charCode: 13 }));
    t.dispatchEvent(new KeyboardEvent('keyup', init));
    if (!down) return 'handled by page';
    // Synthetic key events don't trigger browser defaults; emulate the useful ones.
    switch (k[0]) {
      case 'Enter': {
        const form = t.form || t.closest?.('form');
        if (form && t.tagName !== 'TEXTAREA' || (form && t.getAttribute('role') === 'combobox')) { form.requestSubmit ? form.requestSubmit() : form.submit(); return 'submitted form'; }
        if (t.tagName === 'A' || t.tagName === 'BUTTON') { t.click(); return 'activated'; }
        if (t.isContentEditable || t.tagName === 'TEXTAREA') document.execCommand('insertLineBreak');
        return 'pressed';
      }
      case 'Tab': {
        const f = [...document.querySelectorAll('a[href],button,input,select,textarea,[tabindex]:not([tabindex="-1"])')].filter((n) => !n.disabled && n.offsetParent);
        const i = f.indexOf(t); f[(i + 1) % f.length]?.focus(); return 'moved focus';
      }
      case 'Escape': t.blur?.(); return 'pressed';
      case 'Backspace': document.execCommand('delete'); return 'pressed';
      case 'PageDown': scrollByDir('down', innerHeight * 0.85); return 'scrolled';
      case 'PageUp': scrollByDir('up', innerHeight * 0.85); return 'scrolled';
      case 'End': scroller(null).scrollTo(0, 1e9); return 'scrolled';
      case 'Home': scroller(null).scrollTo(0, 0); return 'scrolled';
      case ' ': if (t === document.body) { scrollByDir('down', innerHeight * 0.85); return 'scrolled'; } document.execCommand('insertText', false, ' '); return 'pressed';
    }
    return 'pressed';
  }

  function type(ref, text, clear, submit) {
    const e = el(ref);
    e.scrollIntoView({ block: 'center', behavior: 'instant' });
    flash(e);
    e.focus();
    const isField = e.tagName === 'INPUT' || e.tagName === 'TEXTAREA';
    if (clear) {
      if (isField) e.select(); else document.execCommand('selectAll');
    } else if (isField) {
      e.setSelectionRange?.(e.value.length, e.value.length);
    }
    // execCommand produces real input events, which frameworks like React listen to.
    const ok = document.execCommand('insertText', false, text);
    if (!ok || (isField && !e.value.includes(text))) {
      if (isField) {
        const proto = e.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
        Object.getOwnPropertyDescriptor(proto, 'value').set.call(e, (clear ? '' : e.value) + text);
        e.dispatchEvent(new InputEvent('input', { bubbles: true, data: text, inputType: 'insertText' }));
      } else {
        e.textContent = (clear ? '' : e.textContent) + text;
        e.dispatchEvent(new InputEvent('input', { bubbles: true, data: text, inputType: 'insertText' }));
      }
    }
    e.dispatchEvent(new Event('change', { bubbles: true }));
    return submit ? key('Enter', e) : 'typed';
  }

  function scroller(from) {
    let n = from || document.elementFromPoint(innerWidth / 2, innerHeight / 2);
    while (n && n !== document.body && n !== document.documentElement) {
      const s = getComputedStyle(n);
      if (/(auto|scroll)/.test(s.overflowY) && n.scrollHeight > n.clientHeight + 4) return n;
      n = n.parentElement;
    }
    return document.scrollingElement || document.documentElement;
  }

  function scrollByDir(dir, amount, ref) {
    const s = scroller(ref ? el(ref) : null);
    s.scrollBy({ top: dir === 'up' ? -amount : amount, behavior: 'instant' });
    const max = Math.max(0, s.scrollHeight - s.clientHeight);
    return `${Math.round(s.scrollTop)}/${Math.round(max)}px`;
  }

  function selectOption(ref, value) {
    const e = el(ref);
    if (e.tagName !== 'SELECT') throw new Error(`${ref} is not a <select>`);
    const want = String(value).toLowerCase();
    const opt = Array.from(e.options).find((o) => o.value.toLowerCase() === want || o.text.trim().toLowerCase() === want)
      || Array.from(e.options).find((o) => o.text.toLowerCase().includes(want));
    if (!opt) throw new Error('No option matching ' + JSON.stringify(value));
    e.value = opt.value;
    e.dispatchEvent(new Event('input', { bubbles: true }));
    e.dispatchEvent(new Event('change', { bubbles: true }));
    return opt.text.trim();
  }

  function readPage(max) {
    const main = document.querySelector('main, [role=main], article') || document.body;
    let text = (main && main.innerText.length > 400 ? main.innerText : document.body?.innerText) || '';
    text = text.replace(/[ \t]+\n/g, '\n').replace(/\n{3,}/g, '\n\n').trim();
    return { url: location.href, title: document.title, text: text.slice(0, max), truncated: text.length > max };
  }

  window.__aside = { v: 5, snapshot, settle, click, type, key, scrollBy: scrollByDir, selectOption, readPage };
})();
