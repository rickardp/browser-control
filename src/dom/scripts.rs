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

/// Performs a fetch from the page context. Receives JSON-string args.
pub const FETCH_JS: &str = r#"
(async function(argsJson) {
    const args = JSON.parse(argsJson);
    const r = await fetch(args.url, {
        method: args.method || 'GET',
        headers: args.headers || {},
        body: args.body,
        credentials: 'include',
    });
    const text = await r.text();
    const headers = {};
    r.headers.forEach((v,k) => { headers[k] = v; });
    return JSON.stringify({ status: r.status, statusText: r.statusText, headers, body: text });
})
"#;
