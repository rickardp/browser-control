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
