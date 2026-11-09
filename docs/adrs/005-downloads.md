---
status: Active
date: 2026-11-08
---

# ADR-005: Silent downloads into a staging folder

## Context

Agents that triggered a download through the browser often stalled. Depending
on the profile, the browser opened a native Save dialog ("Ask where to save",
Firefox's "What should Firefox do with this file?", Chrome's "download
multiple files" prompt, or Save Page As on Cmd/Ctrl+S). The dialog is
invisible to CDP and BiDi, so the agent saw nothing happen and concluded the
server or browser had hung. When the Playwright sidecar was connected the
opposite happened: `connectOverCDP` sets `Browser.setDownloadBehavior
{allowAndName}` into a temp dir it deletes on disconnect, so files vanished
under GUID names. No tool reported that a download started or where it went.

Constraints found while reviewing the first plan:

- `Browser.setDownloadBehavior` is browser-global (per browser context), not
  per connection. When a connection that set it detaches, Chromium resets it
  to the profile default (unconditionally before M154; from M154 only if that
  connection was the last writer). The sidecar, other MCP servers sharing the
  browser, and crashes all detach at arbitrary times, and nothing can observe
  the reset without polling, which the no-idle-work rule forbids.
- Several MCP servers (different agents, different working directories)
  share one browser, so a per-agent download path would put one agent's file
  into another agent's repository.
- `behavior: "allow"` writes the page-suggested name with no dedup and
  overwrites existing files. A page controls that name through
  `Content-Disposition`.
- Setting any download override in Chromium installs a devtools download
  delegate for the profile until restart; Save Page As then silently does
  nothing and Safe Browsing no longer gates completion. Acceptable for a
  browser-control profile, not for a user's own browser.
- The MCP server's working directory is often `/` (GUI hosts) or a git repo.
- Firefox: BiDi `browser.setDownloadBehavior` exists from Firefox 149 (fully
  reliable from 152); download events from 144/145; `handlers.json`
  "always ask" entries still prompt without it.

## Decision

1. **Staging folder.** Downloads from browsers browser-control launched land
   in `<data_dir>/downloads/<kind>/`. It is the default destination; it is
   never the agent's working directory.
2. **Launch-time prefs** for owned profiles only (under
   `<config_dir>/profiles`): Chromium `Default/Preferences`
   (`download.prompt_for_download=false`, `download.default_directory`,
   `download.directory_upgrade`, `savefile.default_directory`,
   `automatic_downloads=1`, no download bubble); Firefox `user.js`
   (`folderList=2`, `dir`, `useDownloadDir`, no "always ask", no panel).
   This covers CLI invocations and every moment without a live connection.
3. **Runtime override, re-sent before tool calls.** The MCP server sends
   Chromium `Browser.setDownloadBehavior {allowAndName, downloadPath: staging,
   eventsEnabled: true}` (Firefox: BiDi `browser.setDownloadBehavior
   {allowed, destinationFolder: staging}`) whenever its backend is used,
   debounced to once per 250 ms except for action tools (click, navigate, …),
   which always re-send. It is also re-sent immediately after the Playwright
   sidecar connects, and on the next tool call after the sidecar is dropped.
   A transient registry error leaves the mode unresolved so the next call
   retries. Every server sends identical values, so the global setting has no
   last-writer problem, and re-sending heals the detach reset. Bounded work
   per call, no timer.
4. **Naming.** Chromium writes `<staging>/<guid>`; on `downloadProgress
   completed` the server renames it to a sanitized name (no path parts,
   control characters, leading dots, reserved Windows names; ≤ 200 bytes),
   never overwriting (`name (1).ext`). Exactly one server performs the rename,
   claimed with an `O_EXCL` marker `.<guid>.claim`; the result is recorded in
   `.<guid>.done` so other servers resolve the path lazily (also after a
   `save_to` move, which rewrites the marker). A completion for a guid this
   server never saw begin (it began before the server connected) is renamed
   too, as `download-<guid prefix>`. Firefox names files uniquely itself and
   reports the path in `downloadEnd`.
5. **Delivery to a specific folder is explicit.** `browser_downloads`
   `wait` / `save` take `save_to` (absolute directory, created if missing);
   `browser-control mcp --download-dir <dir>` or
   `BROWSER_CONTROL_DOWNLOAD_DIR` sets the default. Moving on request avoids
   guessing which agent owns a download when several share a browser.
6. **Reporting.** Action tool results (click, type, press_key, navigate, eval,
   hover, drag, tab_new, select_element) list downloads that began during the
   call. `browser_downloads list|wait|save` reads the server's records.
   `wait` without an id takes the oldest download no earlier `wait`
   returned, not "the download from the last action": a download often
   begins after the click returns and the agent may act again first, and a
   multi-file download is handed out one file per call.
   **Attribution.** Every server sharing a browser sees every download, so
   each download is attributed to a tab: the CDP `frameId` / BiDi `context`,
   followed up the opener/parent chain (CDP `Target.setDiscoverTargets`
   `openerId`, BiDi `contextCreated` `originalOpener` / `parent`). It belongs
   to this server if that chain reaches a tab this server's tools used; a
   frame the browser never announced (a CDP iframe) belongs to whichever
   server has an action running at that moment. Action notes and id-less
   `wait` only consider this server's downloads; `list` marks the others and
   any id still works.
   `wait` returns an unfinished download as `in_progress` rather than
   blocking past its timeout (default 5 s to begin, 30 s to finish, max 120 s).
7. **Unmanaged browsers are not touched.** A browser browser-control did not
   launch, or an external endpoint, keeps its own download settings;
   `browser_downloads` explains why.
8. **Save As and Print chords are refused** by `browser_press_key`
   (`Control/Meta+S`, `Control/Meta+P`) with a hint to `browser_get_html`,
   `browser_pdf_save`, or `browser_curl -o`, unless the call passes
   `allow_native_dialog: true` (for pages that bind the shortcut themselves,
   such as in-app editors).
9. **Failure reporting instead of hangs.** If the event channel overflows,
   in-progress downloads are flagged so `wait` returns instead of blocking
   (the flag clears on any later event); if `<staging>/<guid>` is missing
   because the override was reset before the download began, the file the
   browser named `name` or `name (N)` is used only if it was written after the
   download began and has the reported size, otherwise an error; a failed
   `save_to` move returns the download with the reason; a download started by
   an action that then failed is reported in the error.
10. Stale `.claim` / `.done` markers older than 7 days are removed from
    staging the first time a server uses a backend; never on a timer.
    Downloads are never deleted, whatever their name.
11. **Re-selecting the same browser keeps records.** `browser_start` /
    `browser_select` of the browser already in use (same endpoint) does not
    clear download records; switching to another browser does.

## Consequences

- No Save dialog for downloads in managed browsers, from MCP or CLI. A
  managed browser that was already running when browser-control was
  upgraded gets the launch-time prefs only after a restart; until then only
  MCP sessions (runtime override) avoid the dialog.
- In a user's own browser, the Playwright sidecar (CSS-selector tools,
  `browser_wait_for`, `browser_pdf_save`) still replaces the download
  behavior with its temporary folder while it is connected; not fixed, as
  browser-control leaves unmanaged browsers' download settings alone.
- The agent always learns the absolute path, or that nothing started.
- A download nobody waits for stays in staging under its final name.
- Downloads the browser starts while no MCP server is connected follow the
  launch prefs (staging folder, browser-chosen unique name) and are not in
  any server's records; `browser_downloads list` shows only what that server
  observed.
- Firefox older than 149 relies on `user.js`; types `handlers.json` marks
  "always ask" can still prompt. Older than 145 reports no download events.
- Not handled: downloads in incognito windows or secondary profiles of a
  Chromium browser (the override applies to the default profile only), and
  `showSaveFilePicker` (File System Access API), which is not a download.
