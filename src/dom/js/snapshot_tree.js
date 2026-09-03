/* bc:snapshot */
// Approximate accessibility tree for engines without a native AX-tree
// command (WebDriver BiDi / Firefox). Emits JSON in the shape of CDP's
// `Accessibility.getFullAXTree` so the Rust renderer, `find`, and the ref
// table are shared with the Chromium path.
//
// Element refs are integer ids kept in a page-side registry
// (`window.__bcRefs`); the document token (`window.__bcDocToken`) is a
// random per-document stamp reported as the root's `backendDOMNodeId`.
//
// Called as `(fn)(maxNodes)` through `script.callFunction`; returns a JSON
// string so BiDi's object-depth serialisation limits never apply.
(function (maxNodes) {
  const w = window;
  const doc = document;
  if (!w.__bcDocToken) {
    w.__bcDocToken = 4294967296 + Math.floor(Math.random() * (9007199254740992 - 4294967296));
  }
  if (!w.__bcRefs) w.__bcRefs = { nextId: 1, byId: new Map(), byEl: new WeakMap() };
  const reg = w.__bcRefs;
  const hasWeakRef = typeof WeakRef === 'function';
  function idFor(el) {
    let id = reg.byEl.get(el);
    if (!id) {
      id = reg.nextId++;
      reg.byEl.set(el, id);
      reg.byId.set(id, hasWeakRef ? new WeakRef(el) : el);
    }
    return id;
  }

  const SKIP = new Set(['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE', 'HEAD', 'META', 'LINK', 'TITLE', 'BASE']);
  const SECTIONING = new Set(['ARTICLE', 'SECTION', 'MAIN', 'NAV', 'ASIDE']);
  const NAME_FROM_CONTENT = new Set([
    'button', 'link', 'heading', 'cell', 'columnheader', 'rowheader', 'option', 'tab',
    'menuitem', 'menuitemcheckbox', 'menuitemradio', 'checkbox', 'radio', 'switch',
    'treeitem', 'tooltip', 'LabelText', 'summary',
  ]);
  const CHECKABLE = new Set(['checkbox', 'radio', 'switch', 'menuitemcheckbox', 'menuitemradio']);

  function collapse(s, max) {
    return String(s || '').replace(/\s+/g, ' ').trim().slice(0, max || 200);
  }
  function attr(el, name) {
    const v = el.getAttribute(name);
    return v === null ? '' : v;
  }
  function inputType(el) {
    return (attr(el, 'type') || 'text').toLowerCase();
  }
  function visible(el) {
    if (el.hidden || attr(el, 'aria-hidden') === 'true') return false;
    if (typeof el.checkVisibility === 'function') {
      try { return el.checkVisibility(); } catch (e) { /* fall through */ }
    }
    const cs = getComputedStyle(el);
    return cs.display !== 'none' && cs.visibility !== 'hidden';
  }
  function inSectioning(el) {
    let p = el.parentElement;
    while (p) {
      if (SECTIONING.has(p.tagName.toUpperCase())) return true;
      p = p.parentElement;
    }
    return false;
  }
  function childrenOf(el) {
    if (el.shadowRoot) return el.shadowRoot.childNodes;
    if (el.tagName.toUpperCase() === 'SLOT') {
      const assigned = el.assignedNodes({ flatten: true });
      if (assigned.length) return assigned;
    }
    return el.childNodes;
  }

  function roleOf(el) {
    const aria = attr(el, 'role').trim().split(/\s+/)[0];
    if (aria) return aria;
    const tag = el.tagName.toUpperCase();
    switch (tag) {
      case 'A': case 'AREA': return el.hasAttribute('href') ? 'link' : 'generic';
      case 'BUTTON': return 'button';
      case 'SUMMARY': return 'button';
      case 'INPUT':
        switch (inputType(el)) {
          case 'button': case 'submit': case 'reset': case 'image': return 'button';
          case 'checkbox': return 'checkbox';
          case 'radio': return 'radio';
          case 'search': return 'searchbox';
          case 'range': return 'slider';
          case 'number': return 'spinbutton';
          case 'hidden': return null;
          default: return 'textbox';
        }
      case 'TEXTAREA': return 'textbox';
      case 'SELECT': return (el.multiple || el.size > 1) ? 'listbox' : 'combobox';
      case 'OPTION': return 'option';
      case 'IMG': return 'image';
      case 'H1': case 'H2': case 'H3': case 'H4': case 'H5': case 'H6': return 'heading';
      case 'NAV': return 'navigation';
      case 'MAIN': return 'main';
      case 'ASIDE': return 'complementary';
      case 'FORM': return 'form';
      case 'DIALOG': return 'dialog';
      case 'ARTICLE': return 'article';
      case 'SECTION':
        return el.hasAttribute('aria-label') || el.hasAttribute('aria-labelledby') ? 'region' : 'generic';
      case 'HEADER': return inSectioning(el) ? 'generic' : 'banner';
      case 'FOOTER': return inSectioning(el) ? 'generic' : 'contentinfo';
      case 'UL': case 'OL': case 'MENU': return 'list';
      case 'LI': return 'listitem';
      case 'TABLE': return 'table';
      case 'TR': return 'row';
      case 'TD': return 'cell';
      case 'TH': return 'columnheader';
      case 'P': return 'paragraph';
      case 'LABEL': return 'LabelText';
      case 'IFRAME': case 'FRAME': return 'Iframe';
      case 'DETAILS': case 'FIELDSET': return 'group';
      case 'HR': return 'separator';
      case 'PROGRESS': return 'progressbar';
      case 'METER': return 'meter';
      default: {
        const ce = attr(el, 'contenteditable');
        if (el.hasAttribute('contenteditable') && ce !== 'false') return 'textbox';
        return 'generic';
      }
    }
  }

  // Visible text of a subtree, honouring aria-label / alt on descendants.
  function textOf(node, exclude, budget) {
    budget = budget || { left: 1000 };
    if (budget.left <= 0 || node === exclude) return '';
    if (node.nodeType === 3) {
      const t = node.nodeValue || '';
      budget.left -= t.length;
      return t;
    }
    if (node.nodeType !== 1) return '';
    const el = node;
    const tag = el.tagName.toUpperCase();
    if (SKIP.has(tag) || !visible(el)) return '';
    const al = attr(el, 'aria-label');
    if (al.trim()) return al;
    if ((tag === 'IMG' || tag === 'AREA') && el.hasAttribute('alt')) return attr(el, 'alt');
    let out = '';
    for (const c of childrenOf(el)) out += textOf(c, exclude, budget) + ' ';
    return out;
  }

  function nameOf(el, role) {
    const tag = el.tagName.toUpperCase();
    const lb = attr(el, 'aria-labelledby');
    if (lb.trim()) {
      const parts = lb.trim().split(/\s+/).map((id) => {
        const t = el.ownerDocument.getElementById(id);
        return t ? textOf(t) : '';
      });
      const joined = collapse(parts.join(' '));
      if (joined) return { name: joined, from: 'labelledby' };
    }
    const al = attr(el, 'aria-label');
    if (al.trim()) return { name: collapse(al), from: 'aria-label' };
    if (el.labels && el.labels.length) {
      const t = collapse(Array.from(el.labels).map((l) => textOf(l, el)).join(' '));
      if (t) return { name: t, from: 'label' };
    }
    if (tag === 'IMG' || tag === 'AREA' || (tag === 'INPUT' && inputType(el) === 'image')) {
      if (el.hasAttribute('alt')) return { name: collapse(attr(el, 'alt')), from: 'alt' };
    }
    if (tag === 'INPUT') {
      const type = inputType(el);
      if (type === 'submit' || type === 'reset' || type === 'button') {
        const v = el.value || (type === 'submit' ? 'Submit' : type === 'reset' ? 'Reset' : '');
        if (v) return { name: collapse(v), from: 'value' };
      }
    }
    if (tag === 'FIELDSET') {
      const lg = el.querySelector(':scope > legend');
      if (lg) return { name: collapse(textOf(lg)), from: 'legend' };
    }
    if (tag === 'TABLE') {
      const c = el.querySelector(':scope > caption');
      if (c) return { name: collapse(textOf(c)), from: 'caption' };
    }
    if (tag === 'FIGURE') {
      const c = el.querySelector(':scope > figcaption');
      if (c) return { name: collapse(textOf(c)), from: 'figcaption' };
    }
    if (tag === 'SVG') {
      const t = el.querySelector('title');
      if (t) return { name: collapse(t.textContent), from: 'title' };
    }
    if (NAME_FROM_CONTENT.has(role)) {
      const t = collapse(textOf(el));
      if (t) return { name: t, from: 'content' };
    }
    const title = attr(el, 'title');
    if (title.trim()) return { name: collapse(title), from: 'title' };
    const ph = attr(el, 'placeholder');
    if (ph.trim()) return { name: collapse(ph), from: 'placeholder' };
    return { name: '', from: null };
  }

  function descriptionOf(el, from) {
    const db = attr(el, 'aria-describedby');
    if (db.trim()) {
      const parts = db.trim().split(/\s+/).map((id) => {
        const t = el.ownerDocument.getElementById(id);
        return t ? textOf(t) : '';
      });
      const joined = collapse(parts.join(' '));
      if (joined) return joined;
    }
    if (from !== 'title' && attr(el, 'title').trim()) return collapse(attr(el, 'title'));
    if (from !== 'placeholder' && attr(el, 'placeholder').trim()) return collapse(attr(el, 'placeholder'));
    return '';
  }

  function valueOf(el, role) {
    const tag = el.tagName.toUpperCase();
    if (tag === 'INPUT') {
      const type = inputType(el);
      if (['password', 'checkbox', 'radio', 'file', 'submit', 'button', 'reset', 'image', 'hidden'].includes(type)) return '';
      return collapse(el.value);
    }
    if (tag === 'TEXTAREA') return collapse(el.value);
    if (tag === 'SELECT') {
      const o = el.selectedOptions && el.selectedOptions[0];
      return o ? collapse(o.label || o.textContent) : '';
    }
    if (role === 'slider' || role === 'spinbutton' || role === 'progressbar' || role === 'meter') {
      return collapse(attr(el, 'aria-valuetext') || attr(el, 'aria-valuenow') || el.value || '');
    }
    if (role === 'textbox' && el.isContentEditable) return collapse(el.textContent);
    return '';
  }

  function propsOf(el, role) {
    const props = [];
    const push = (n, v) => props.push({ name: n, value: { value: v } });
    const tag = el.tagName.toUpperCase();
    let disabled = attr(el, 'aria-disabled') === 'true';
    try { disabled = disabled || el.matches(':disabled'); } catch (e) { /* ignore */ }
    if (disabled) push('disabled', true);
    const focusable = !disabled && (el.tabIndex >= 0 || el.hasAttribute('tabindex'));
    if (focusable) push('focusable', true);
    if (CHECKABLE.has(role)) {
      let c;
      if (el.indeterminate) c = 'mixed';
      else if (tag === 'INPUT') c = el.checked ? 'true' : 'false';
      else {
        const a = attr(el, 'aria-checked');
        c = a === 'true' ? 'true' : a === 'mixed' ? 'mixed' : 'false';
      }
      push('checked', c);
    }
    const pressed = attr(el, 'aria-pressed');
    if (pressed === 'true' || pressed === 'mixed') push('pressed', pressed);
    if (attr(el, 'aria-expanded') === 'true') push('expanded', true);
    else if (tag === 'SUMMARY' && el.parentElement && el.parentElement.tagName.toUpperCase() === 'DETAILS' && el.parentElement.open) push('expanded', true);
    if (tag === 'OPTION' ? el.selected : attr(el, 'aria-selected') === 'true') push('selected', true);
    try { if (el.getRootNode().activeElement === el) push('focused', true); } catch (e) { /* ignore */ }
    if (el.required || attr(el, 'aria-required') === 'true') push('required', true);
    if (el.readOnly || attr(el, 'aria-readonly') === 'true') push('readonly', true);
    if (role === 'heading') {
      const m = /^H([1-6])$/.exec(tag);
      const lvl = attr(el, 'aria-level') || (m ? m[1] : '');
      if (lvl) push('level', Number(lvl));
    }
    if (role === 'link' && el.href) push('url', String(el.href).slice(0, 2048));
    return props;
  }

  const nodes = [];
  let count = 0;
  let truncated = false;
  let textSeq = 0;
  const root = {
    nodeId: 'root',
    backendDOMNodeId: w.__bcDocToken,
    role: { value: 'RootWebArea' },
    name: { value: collapse(doc.title) },
    childIds: [],
  };
  nodes.push(root);

  function emitText(parent, text) {
    const t = collapse(text, 1000);
    if (!t) return;
    if (count >= maxNodes) { truncated = true; return; }
    const id = 't' + (++textSeq);
    nodes.push({ nodeId: id, parentId: parent.nodeId, role: { value: 'StaticText' }, name: { value: t }, childIds: [] });
    parent.childIds.push(id);
    count++;
  }

  function emitLeaf(el, parent, role, name) {
    if (count >= maxNodes) { truncated = true; return; }
    const num = idFor(el);
    const id = 'n' + num;
    const n = { nodeId: id, parentId: parent.nodeId, backendDOMNodeId: num, role: { value: role }, name: { value: name }, childIds: [] };
    const props = propsOf(el, role);
    if (props.length) n.properties = props;
    nodes.push(n);
    parent.childIds.push(id);
    count++;
  }

  function walk(node, parent) {
    if (truncated) return;
    if (node.nodeType === 3) { emitText(parent, node.nodeValue); return; }
    if (node.nodeType !== 1) return;
    const el = node;
    const tag = el.tagName.toUpperCase();
    if (SKIP.has(tag)) return;
    if (tag !== 'OPTION' && !visible(el)) return;
    if (tag === 'SVG') {
      const nm = nameOf(el, 'image');
      if (nm.name) emitLeaf(el, parent, 'image', nm.name);
      return;
    }
    if (tag === 'IFRAME' || tag === 'FRAME') {
      emitLeaf(el, parent, 'Iframe', nameOf(el, 'Iframe').name);
      return;
    }
    const role = roleOf(el);
    if (role === null) return;
    const nm = nameOf(el, role);
    const value = valueOf(el, role);
    const props = propsOf(el, role);
    const emit = !(role === 'generic' && !nm.name && !value && props.length === 0);
    let cur = parent;
    if (emit) {
      if (count >= maxNodes) { truncated = true; return; }
      const num = idFor(el);
      const id = 'n' + num;
      const n = { nodeId: id, parentId: parent.nodeId, backendDOMNodeId: num, role: { value: role }, name: { value: nm.name }, childIds: [] };
      if (value) n.value = { value: value };
      const desc = descriptionOf(el, nm.from);
      if (desc) n.description = { value: desc };
      if (props.length) n.properties = props;
      nodes.push(n);
      parent.childIds.push(id);
      count++;
      cur = n;
    }
    // Form controls have no meaningful children; everything else recurses.
    if (tag === 'INPUT' || tag === 'TEXTAREA') return;
    for (const c of childrenOf(el)) walk(c, cur);
  }

  const start = doc.body || doc.documentElement;
  if (start) for (const c of childrenOf(start)) walk(c, root);
  return JSON.stringify({ nodes: nodes, truncated: truncated });
})
