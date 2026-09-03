//! JavaScript payloads injected into the browser by MCP tools.

/// Serializes the DOM with shadow roots included.
pub const GET_DOM_JS: &str = r#"
(function(selector) {
    const root = selector ? document.querySelector(selector) : document.documentElement;
    if (!root) return null;
    if (typeof root.getHTML === 'function') {
        try { return root.getHTML({ serializableShadowRoots: true }); } catch (e) {}
    }
    return root.outerHTML;
})
"#;

/// Resolves a selector to a clip rectangle in *document* coordinates,
/// scrolling the element into view first. Returns `null` when the selector
/// matches nothing or the element has zero area (hidden / detached).
pub const GET_CLIP_RECT_JS: &str = r#"
(function(selector) {
    const el = document.querySelector(selector);
    if (!el) return null;
    el.scrollIntoView({ block: 'center', inline: 'center' });
    const r = el.getBoundingClientRect();
    if (r.width === 0 || r.height === 0) return null;
    return {
        x: r.left + window.scrollX,
        y: r.top + window.scrollY,
        width: r.width,
        height: r.height,
    };
})
"#;

/// Interactive element picker. Resolves with the selector string for the picked element.
pub const SELECT_ELEMENT_JS: &str = r#"
(function() {
    function cssPath(el) {
        if (!(el instanceof Element)) return '';
        const path = [];
        while (el && el.nodeType === 1) {
            let selector = el.nodeName.toLowerCase();
            if (el.id) { selector += '#' + el.id; path.unshift(selector); break; }
            else {
                let sib = el, nth = 1;
                while ((sib = sib.previousElementSibling)) { if (sib.nodeName === el.nodeName) nth++; }
                selector += ':nth-of-type(' + nth + ')';
            }
            path.unshift(selector);
            el = el.parentNode;
        }
        return path.join(' > ');
    }
    return new Promise((resolve) => {
        const overlay = document.createElement('div');
        overlay.style.cssText = 'position:fixed;inset:0;z-index:2147483647;cursor:crosshair;background:rgba(0,150,255,0.05);';
        document.body.appendChild(overlay);
        overlay.addEventListener('click', (e) => {
            e.preventDefault(); e.stopPropagation();
            overlay.remove();
            const x = e.clientX, y = e.clientY;
            const el = document.elementFromPoint(x, y);
            resolve(cssPath(el));
        }, { capture: true, once: true });
    });
})()
"#;

/// Readable-text extraction for `browser_get_page_text`. Called as
/// `(fn)(maxChars, selectorOrNull)`; returns a JSON string
/// `{title, url, source, text, truncated, total_chars}` or `{error}`.
///
/// Root selection: explicit selector → `main`/`article`/`[role=main]` →
/// the largest text block with the lowest link density → `body`. Page
/// chrome (nav/header/footer/aside and their ARIA equivalents), scripts,
/// styles, hidden elements, and form controls are skipped; headings, list
/// items, and table cells keep a little structure.
pub const GET_PAGE_TEXT_JS: &str = r#"
(function(maxChars, selector) {
    const doc = document;
    const SKIP = new Set(['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE', 'SVG', 'CANVAS', 'IFRAME', 'OBJECT', 'EMBED', 'INPUT', 'TEXTAREA', 'SELECT']);
    const CHROME = new Set(['NAV', 'HEADER', 'FOOTER', 'ASIDE']);
    const CHROME_ROLES = new Set(['navigation', 'banner', 'contentinfo', 'complementary']);
    const BLOCK = new Set(['P', 'DIV', 'LI', 'TR', 'H1', 'H2', 'H3', 'H4', 'H5', 'H6', 'SECTION', 'ARTICLE', 'BLOCKQUOTE', 'PRE', 'DT', 'DD', 'BR', 'HR', 'UL', 'OL', 'TABLE', 'MAIN', 'FORM', 'FIGURE', 'FIGCAPTION', 'DETAILS', 'SUMMARY', 'LABEL', 'OPTION', 'TD', 'TH']);
    function visible(el) {
        if (el.hidden || el.getAttribute('aria-hidden') === 'true') return false;
        try { if (typeof el.checkVisibility === 'function') return el.checkVisibility(); } catch (e) {}
        const cs = getComputedStyle(el);
        return cs.display !== 'none' && cs.visibility !== 'hidden';
    }
    let root = null, source = 'body';
    if (selector) {
        root = doc.querySelector(selector);
        if (!root) return JSON.stringify({ error: 'selector matched no element: ' + selector });
        source = 'selector';
    } else {
        root = doc.querySelector('main, [role="main"], article');
        if (root) {
            source = root.tagName === 'ARTICLE' ? 'article' : 'main';
        } else {
            let best = null, bestScore = 0;
            for (const el of doc.querySelectorAll('article, section, div')) {
                const len = (el.innerText || '').length;
                if (len < 200) continue;
                let linkLen = 0;
                for (const a of el.querySelectorAll('a')) linkLen += (a.innerText || '').length;
                const score = len * (1 - Math.min(1, linkLen / len));
                if (score > bestScore) { bestScore = score; best = el; }
            }
            if (best) { root = best; source = 'heuristic'; }
            else root = doc.body || doc.documentElement;
        }
    }
    const parts = [];
    function walk(node, isRoot) {
        if (node.nodeType === 3) {
            const t = node.nodeValue;
            if (t && t.trim()) parts.push(t.replace(/\s+/g, ' '));
            return;
        }
        if (node.nodeType !== 1) return;
        const tag = node.tagName;
        if (SKIP.has(tag)) return;
        if (!isRoot && (CHROME.has(tag) || CHROME_ROLES.has(node.getAttribute('role')))) return;
        if (!visible(node)) return;
        const block = BLOCK.has(tag);
        if (block) parts.push('\n');
        if (/^H[1-6]$/.test(tag)) parts.push('#'.repeat(+tag[1]) + ' ');
        else if (tag === 'LI') parts.push('- ');
        const kids = node.shadowRoot ? node.shadowRoot.childNodes : node.childNodes;
        for (const c of kids) walk(c, false);
        if (tag === 'TD' || tag === 'TH') parts.push(' | ');
        if (block) parts.push('\n');
    }
    walk(root, true);
    let text = parts.join('')
        .replace(/[ \t]+\n/g, '\n')
        .replace(/\n[ \t]+/g, '\n')
        .replace(/[ \t]{2,}/g, ' ')
        .replace(/\n{3,}/g, '\n\n')
        .trim();
    const total = text.length;
    let truncated = false;
    if (text.length > maxChars) {
        let cut = text.lastIndexOf('\n', maxChars);
        if (cut < maxChars / 2) cut = maxChars;
        text = text.slice(0, cut);
        truncated = true;
    }
    return JSON.stringify({ title: doc.title, url: location.href, source, text, truncated, total_chars: total });
})
"#;

/// Called via `Runtime.callFunctionOn` with `this` bound to the element
/// about to receive typed text. Selects the element's current content so
/// the following `Input.insertText` replaces it (like Playwright `fill`).
/// With `clear=true` the content is emptied outright and `input`/`change`
/// are dispatched, for the "type an empty string" case.
pub const SELECT_ALL_JS: &str = r#"
(function(clear) {
    const el = this;
    const tag = (el.tagName || '').toLowerCase();
    if (tag === 'input' || tag === 'textarea') {
        if (clear) {
            el.value = '';
            el.dispatchEvent(new Event('input', { bubbles: true }));
            el.dispatchEvent(new Event('change', { bubbles: true }));
        } else {
            try { el.select(); } catch (e) {
                try { el.setSelectionRange(0, el.value.length); } catch (e2) {}
            }
        }
        return 'field';
    }
    if (el.isContentEditable) {
        if (clear) {
            el.textContent = '';
            el.dispatchEvent(new InputEvent('input', { bubbles: true }));
        } else {
            const sel = window.getSelection();
            sel.removeAllRanges();
            const range = document.createRange();
            range.selectNodeContents(el);
            sel.addRange(range);
        }
        return 'contenteditable';
    }
    return 'other';
})
"#;

// ---------------------------------------------------------------------------
// WebDriver BiDi (Firefox) accessibility snapshot + ref helpers.
//
// BiDi has no accessibility-tree command, so `SNAPSHOT_TREE_JS` walks the
// DOM and emits `Accessibility.getFullAXTree`-shaped JSON with element refs
// held in a page-side registry (`window.__bcRefs`). The helpers below take
// a registry id and return a JSON string: `{"gone":true}` when the element
// is no longer in the document, `{"error":"…"}` for a real failure.
// ---------------------------------------------------------------------------

/// The walker itself; see the header comment in the file.
pub const SNAPSHOT_TREE_JS: &str = include_str!("js/snapshot_tree.js");

/// Expression (for `script.evaluate`) yielding the per-document token as a
/// decimal string; creates it when the walker has not run yet.
pub const DOC_TOKEN_JS: &str = "String(window.__bcDocToken ?? (window.__bcDocToken = 4294967296 + Math.floor(Math.random() * (9007199254740992 - 4294967296))))";

/// Expression yielding the document's scroll size as JSON, for full-page
/// screenshots on BiDi.
pub const DOC_SIZE_JS: &str = "JSON.stringify({width: Math.max(document.documentElement.scrollWidth, document.body ? document.body.scrollWidth : 0), height: Math.max(document.documentElement.scrollHeight, document.body ? document.body.scrollHeight : 0)})";

/// `(id)` → centre of the element in viewport CSS px after scrolling it
/// into view, clipped to the viewport.
pub const REF_CENTER_JS: &str = r#"
/* bc:center */
(function(id) {
    const reg = window.__bcRefs;
    const r = reg && reg.byId.get(Number(id));
    const el = r && typeof r.deref === 'function' ? r.deref() : r;
    if (!el || !el.isConnected) return JSON.stringify({ gone: true });
    try { el.scrollIntoView({ block: 'center', inline: 'center' }); } catch (e) {}
    const rect = el.getBoundingClientRect();
    const vw = window.innerWidth, vh = window.innerHeight;
    const x1 = Math.max(0, rect.left), y1 = Math.max(0, rect.top);
    const x2 = Math.min(vw, rect.right), y2 = Math.min(vh, rect.bottom);
    if (x2 - x1 <= 1 || y2 - y1 <= 1) {
        return JSON.stringify({ error: 'element has no visible box (hidden, zero-size, or outside the viewport after scrolling)' });
    }
    return JSON.stringify({ x: (x1 + x2) / 2, y: (y1 + y2) / 2 });
})
"#;

/// `(id)` → the element's border box in document coordinates (same
/// contract as `GET_CLIP_RECT_JS`).
pub const REF_CLIP_RECT_JS: &str = r#"
/* bc:clip */
(function(id) {
    const reg = window.__bcRefs;
    const r = reg && reg.byId.get(Number(id));
    const el = r && typeof r.deref === 'function' ? r.deref() : r;
    if (!el || !el.isConnected) return JSON.stringify({ gone: true });
    try { el.scrollIntoView({ block: 'center', inline: 'center' }); } catch (e) {}
    const rect = el.getBoundingClientRect();
    if (rect.width === 0 || rect.height === 0) return JSON.stringify({ error: 'element has zero area' });
    return JSON.stringify({
        x: rect.left + window.scrollX,
        y: rect.top + window.scrollY,
        width: rect.width,
        height: rect.height,
    });
})
"#;

/// `(id, text, mode)` with `mode` = `fill` | `select` | `clear`: focus the
/// element, select its content (or clear it), and for `fill` insert `text`
/// through `execCommand('insertText')` with a value-setter fallback that
/// keeps framework value trackers in sync. Returns `{kind, method}`.
pub const REF_TYPE_JS: &str = r#"
/* bc:type */
(function(id, text, mode) {
    const reg = window.__bcRefs;
    const r = reg && reg.byId.get(Number(id));
    const el = r && typeof r.deref === 'function' ? r.deref() : r;
    if (!el || !el.isConnected) return JSON.stringify({ gone: true });
    try { el.focus(); } catch (e) {}
    const tag = (el.tagName || '').toLowerCase();
    const isField = tag === 'input' || tag === 'textarea';
    const editable = !isField && el.isContentEditable;
    const kind = isField ? 'field' : editable ? 'contenteditable' : 'other';
    function setValue(v) {
        const proto = tag === 'textarea' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
        const d = Object.getOwnPropertyDescriptor(proto, 'value');
        if (d && d.set) d.set.call(el, v); else el.value = v;
        el.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertText', data: v }));
        el.dispatchEvent(new Event('change', { bubbles: true }));
    }
    if (mode === 'clear') {
        if (isField) setValue('');
        else if (editable) { el.textContent = ''; el.dispatchEvent(new InputEvent('input', { bubbles: true })); }
        return JSON.stringify({ kind: kind, method: 'clear' });
    }
    if (isField) {
        try { el.select(); } catch (e) { try { el.setSelectionRange(0, el.value.length); } catch (e2) {} }
    } else if (editable) {
        const sel = window.getSelection();
        sel.removeAllRanges();
        const range = document.createRange();
        range.selectNodeContents(el);
        sel.addRange(range);
    }
    if (mode === 'select') return JSON.stringify({ kind: kind, method: 'select' });
    let ok = false;
    try { ok = document.execCommand('insertText', false, text); } catch (e) { ok = false; }
    if (isField && el.value !== text) ok = false;
    let method = 'execCommand';
    if (!ok) {
        method = 'fallback';
        if (isField) setValue(text);
        else if (editable) {
            el.textContent = text;
            el.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertText', data: text }));
        }
    }
    return JSON.stringify({ kind: kind, method: method });
})
"#;

/// Performs a fetch from the page context. Receives JSON-string args.
pub const FETCH_JS: &str = r#"
(async function(argsJson) {
    const args = JSON.parse(argsJson);
    const timeoutMs = Number(args.timeoutMs || 0);
    const controller = timeoutMs > 0 && typeof AbortController !== 'undefined'
        ? new AbortController()
        : null;
    const timer = controller
        ? setTimeout(() => controller.abort(), timeoutMs)
        : null;
    try {
        const r = await fetch(args.url, {
            method: args.method || 'GET',
            headers: args.headers || {},
            body: args.body,
            credentials: 'include',
            signal: controller ? controller.signal : undefined,
        });
        const text = await r.text();
        const headers = {};
        r.headers.forEach((v,k) => { headers[k] = v; });
        return JSON.stringify({ ok: true, status: r.status, statusText: r.statusText, headers, body: text });
    } catch (e) {
        const aborted = controller && controller.signal.aborted;
        return JSON.stringify({
            ok: false,
            error: aborted ? `fetch timed out after ${timeoutMs}ms` : String(e),
            errorName: e && e.name ? e.name : null,
        });
    } finally {
        if (timer) clearTimeout(timer);
    }
})
"#;
