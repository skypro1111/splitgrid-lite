# SplitGrid Lite Browser Control (Chrome MV3)

Lets an agent running in a SplitGrid Lite terminal drive **one tab you
explicitly hand over** in your real browser — your real logins and session —
as an alternative to Lite's embedded browser pane. The agent targets it with
`splitgrid-browser --target tab:c1.<tabId> …` (see the sg-browser skill).

This is a copy of the SplitGrid (Electron) app's extension, adapted for Lite:

| | SplitGrid app | SplitGrid Lite (this one) |
| --- | --- | --- |
| Name | SplitGrid Browser Control | SplitGrid Lite Browser Control |
| Connects to | `ws://127.0.0.1:19558/ext` | `ws://127.0.0.1:19568/ext` |
| Pairing stored by the desktop in | `browser-extension-pairings.json` | `lite-browser-extension-pairings.json` |
| In-page marker | `data-splitgrid-control` | `data-splitgrid-lite-control` |
| Toolbar icon | SplitGrid | SplitGrid with a blue dot |

Both can be installed side by side; each pairs with its own app. The wire
protocol is vendored in `src/ext-protocol.ts` (the Rust half is
`src/agent/ext_protocol.rs`), so this directory builds on its own.

## Build

```sh
cd extension
npm install
npm run build        # → extension/dist   (npm run watch to rebuild on change)
npm run typecheck    # optional
```

## Install (Load unpacked)

1. Open `chrome://extensions` and turn on **Developer mode** (top right).
2. **Load unpacked** → select this `extension/dist` folder.
3. Chrome warns that the extension can *"Debug and read data from attached
   debug targets"*. That is the `debugger` permission, used only for
   screenshots (see below).
4. Optional: pin it from the puzzle-piece menu so the icon stays in the toolbar.

Works in Chrome / Edge / Brave / Opera / Vivaldi 120+. After a rebuild, press
the reload arrow on the extension's card in `chrome://extensions`.

## Pair

1. In SplitGrid Lite: **Settings → General → Agent integrations**. Turn on
   *Enable agent integrations*, then *"Let agents inspect a tab in your real
   browser"*, and press **Pair browser extension**. It shows a 6-digit code,
   valid for 3 minutes.
2. Click the extension's toolbar icon and type the code.

Settings then lists the browser as connected. The pairing token the extension
gets back is stored in `chrome.storage.local` (never `storage.sync`); "Unpair"
in the popup deletes it, and **Unpair** in Lite's Settings revokes it on the
desktop side (the popup then needs a fresh pair). Until a token exists, or a
code is typed, the extension opens no socket at all.

Lite must be listening on 19568. If something else held that port when Lite
started, Lite falls back to another port and Settings says so; the extension
cannot reach it until Lite is restarted with 19568 free.

## Activate a tab

The popup lists your open tabs. **Activate** on one:

* requests the host permission for *that tab's host only*, through
  `chrome.permissions.request()` under your click;
* records the tab's full origin (scheme + host + **port**) as the grant;
* adds it to the activated set, which is pushed to SplitGrid Lite so the agent can
  target it as `tab:c1.<tabId>`.

Each activated tab has an **Allow interaction** switch. It is **off** by default:

| Switch | Agent can do |
| --- | --- |
| off (default) | `info`, `text`, `html`, `screenshot`, `release` |
| on | the above **plus** `navigate`, `back`, `forward`, `reload`, `eval` |

`eval` counts as a write op even though it looks like a read: it is arbitrary JS
in a page carrying your session, so it can click, submit and exfiltrate whatever
its author intended. Both ends enforce this — SplitGrid Lite's `agentChromeWrite`
setting *and* this switch must both be on.

## The control indicator

An activated tab is marked, in the page itself, so you never have to open the
popup to know which tab is exposed and how far:

| | Frame | Corner label (top right) | Toolbar badge |
| --- | --- | --- | --- |
| read-only | 3 px **dashed** blue `#2f6fed` | hollow ring + `SplitGrid Lite · agent watching · READ-ONLY` | `READ`, blue |
| interactive | 5 px **solid** orange `#e8590c` | filled dot + `SplitGrid Lite · agent watching · CAN ACT` | `ACT`, orange |

Colour is never the only cue — dashed vs solid, thin vs thick and hollow vs
filled all move together, so the two states stay distinguishable without hue.
The frame is `position: fixed` at the viewport edges with `pointer-events: none`,
so it never shifts layout, never scrolls with the page and never eats a click.
There is no idle animation; the border flashes once (620 ms) when a command
actually executes, and not at all under `prefers-reduced-motion: reduce`.

The page cannot remove or restyle it. The host element is mounted on
`<html>` with a **closed** shadow root, every style lives inside that root, and
the host's own geometry is set as `!important` *inline* — the one author-level
declaration that outranks any page stylesheet, including `* { all: unset }`. A
`MutationObserver` in the extension's isolated world puts the host back if page
script deletes it and restores its attributes if they are rewritten. The host
carries a stable `data-splitgrid-lite-control` attribute so its *presence* stays
externally checkable (`document.querySelector('[data-splitgrid-lite-control]')`)
while its contents stay sealed. It is `aria-hidden`: the popup is the accessible
source of truth, and this frame is non-interactive decoration.

It appears and disappears with real control — injected on activate and after
every navigation, reload and SPA history entry within the granted origin;
updated in place (no reload) when **Allow interaction** flips; removed on
deactivate, `release`, cross-origin revocation, host-permission removal, tab
close, and **when the desktop socket drops**, because a tab nobody can drive
must not claim it is under control. On every service-worker wake the frames are
reconciled against the activated set, so an eviction can leave neither a stale
frame nor a missing one.

**Screenshots are not marked.** `screenshot` hides the frame for the duration of
the CDP capture and restores it in a `finally`, so the agent gets the page as
you see it and a failed capture cannot leave a controlled tab looking unmarked.
The host element stays in the DOM throughout — the tab *is* still controlled, so
the externally checkable attribute keeps saying so; only the pixels go away. The
frame is suppressed under `@media print` for the same reason.

## Permission model

The manifest ships **no `host_permissions` at all**. Only
`optional_host_permissions: ["*://*/*"]`, which grants nothing by itself — it is
the *set of hosts the extension is allowed to ask about*. A freshly installed,
freshly paired extension can read exactly zero pages.

* Chrome match patterns have no port component, so the grant Chrome records is
  host-wide (`https://example.com/*`). The extension pins the tab to the **full
  origin** including the port, and the stricter of the two is what gates every
  command. `http://localhost:3000` does not give an agent `http://localhost:8080`.
* **Cross-origin navigation revokes control.** On every `tabs.onUpdated` the
  tab's current origin is compared to the granted one; a mismatch drops the tab
  from the activated set and pushes the new list to the desktop. Following a
  link off `localhost` and into your mail ends agent control — silently and
  immediately — and you must re-activate to get it back.
* Closing the tab, a prerender swapping the tab id, revoking the host from
  `chrome://extensions`, and `release` all drop it too.
* Before *every* command the extension re-checks: still activated? origin still
  matching? permission still held (asked of Chrome, not of a cache)? write op
  allowed by the switch? The desktop checks its own copy of these rules first;
  neither side trusts the other to have done it.
* **Deactivate** / `release` also hands the host permission back
  (`chrome.permissions.remove`), unless another activated tab still needs it.

## Deliberately not possible

| | Why |
| --- | --- |
| `chrome://*`, `edge://*`, `about:`, `devtools://`, `view-source:` | Chrome forbids extension scripting on its own surfaces. Not a policy choice we could relax. |
| Chrome Web Store (`chromewebstore.google.com`, `chrome.google.com`) | Chrome blocks all extensions there so a page cannot install things for you. |
| Other extensions' pages (`chrome-extension://`) | Same origin isolation that keeps extensions apart from each other. |
| `file://` URLs | Needs the separate "Allow access to file URLs" toggle, which `optional_host_permissions` cannot request. Not supported. |
| The built-in PDF viewer | The visible document is a plugin surface, not a DOM the extension can read. `screenshot` may work; `text`/`html` will return the wrapper, not the PDF. |
| Screenshotting a tab whose DevTools are open | `chrome.debugger` allows one client per tab. The command fails with `debugger_busy`; close DevTools and retry. |
| Any tab you have not activated | There is no permission for it, so there is nothing to fall back to. |

Screenshots use `chrome.debugger` + CDP `Page.captureScreenshot` rather than
`chrome.tabs.captureVisibleTab`, because the latter only ever captures the
*active tab of a focused window* — useless for a background tab an agent drives
while you work elsewhere. The debugger is attached for the duration of one
capture and detached immediately, so Chrome's "started debugging this browser"
infobar appears only for that moment.

## MV3 lifetime

The service worker is evicted after ~30 s idle, so:

* nothing lives only in memory — the activated set is in
  `chrome.storage.session`, the token in `chrome.storage.local`;
* an open WebSocket keeps the worker alive, and the desktop pings every 20 s
  (`EXT_HEARTBEAT_MS`), which covers the connected case;
* once the socket is gone nothing would wake us to reconnect, so a
  `chrome.alarms` timer at the 30 s platform minimum is the floor under the
  in-memory exponential backoff;
* on every wake the worker reconciles the stored activated set against the live
  browser (tab closed? navigated away? permission revoked?) before anything can
  act on it.
