---
name: sg-browser
description: Drives the browser pane embedded in SplitGrid through $SPLITGRID_BROWSER_CLI - open a URL, a localhost dev server or a local HTML file, screenshot it, read console errors and network requests, inspect or extract the DOM, run JS, and click, fill, type and scroll through pages and forms. Use it whenever a web UI or HTML file is built, changed or debugged from a SplitGrid terminal, to see the real result instead of guessing, and for any request to open, render, check, test, verify, screenshot, scrape or automate a page, visual checks inside a subagent task included, even when the user doesn't mention the pane. Also use it to show the user a result as a page - a report, comparison, diagram, chart or table that reads better than terminal text. It replaces headless Chrome (--headless --screenshot), Playwright, puppeteer, Claude in Chrome and other browser tools here, which render where the user can't see. Requires a SplitGrid terminal ($SPLITGRID_BROWSER_CLI set).
disallowed-tools: mcp__claude-in-chrome__*
---

# SplitGrid browser control

SplitGrid has an embedded Chromium pane that the shell can drive: open a page
or a local HTML file, look at it, read its console, inspect the DOM and
interact with it. Use this CLI for every browser action in a SplitGrid
terminal, a subagent's visual check included. Headless Chrome, Playwright and
other browser tools render where the user can't see, and Claude in Chrome is
disabled while this skill is active.

```sh
"$SPLITGRID_BROWSER_CLI" <command> [args...]
```

If `$SPLITGRID_BROWSER_CLI` is empty, this is not a SplitGrid terminal, and this
skill does not apply. Each call prints one JSON line: `{"ok":true, ...}` or
`{"ok":false,"error":"...","message":"..."}`. Check `ok` after every call.

## Verify a change

```sh
"$SPLITGRID_BROWSER_CLI" open http://localhost:3000   # focus the browser pane you own, or create one
"$SPLITGRID_BROWSER_CLI" wait load
"$SPLITGRID_BROWSER_CLI" screenshot                   # -> {"ok":true,"screenshot":"/.../r12.png"}
"$SPLITGRID_BROWSER_CLI" get console                  # recent console messages, errors included
"$SPLITGRID_BROWSER_CLI" eval "document.querySelector('h1')?.textContent"
```

- `open` takes a URL, `localhost:5173` (gets `http://`), or a local file path,
  relative or absolute (loads as `file://`). A static HTML file needs no server.
- `screenshot` returns a PNG **path**. Read that file to see the page.
- Always `wait` (`load`, `selector`, `text`, `url`) after `open` or an action,
  before `screenshot`, `get` or `eval`. Otherwise the page may be half-rendered.
- Prefer `eval` and `get console` for exact assertions. Use `screenshot` when
  layout or visual state matters.

## Show a result as a page

When an answer reads better as a page than as terminal text, write it as an
HTML file and open it in the browser pane. Good fits include a report or
summary with sections, a comparison of options, an architecture or flow
diagram, a chart, a table with more than a few rows or columns, and a UI
mockup. A short answer, a single command or a code change stays in the
terminal.

```sh
"$SPLITGRID_BROWSER_CLI" open "$dir/report.html"
"$SPLITGRID_BROWSER_CLI" wait load
"$SPLITGRID_BROWSER_CLI" get console        # no errors?
"$SPLITGRID_BROWSER_CLI" screenshot         # read the PNG: does it look right?
```

- **Where.** Put the file in your scratchpad or temp directory, not in the
  project, unless the user asks to keep it there. Give each page a telling
  name, such as `auth-options.html` rather than `out.html`.
- **One self-contained file.** Inline the CSS and your own JS. Load libraries
  from a CDN with a pinned version, for example Mermaid for diagrams, Chart.js
  for charts, and marked for Markdown. Don't reference sibling files.
- **Dark by default.** The pane sits inside a dark app. Use a dark background,
  light text, a system font stack, and a readable max width.
- **Check before you point the user at it.** After `open`, run `get console`
  and look at a `screenshot`. Fix errors, empty diagrams and overflowing
  tables first.
- **Update in place.** Rewrite the file and run `reload` (then `wait load`),
  so the user's pane keeps showing the page. Write a new file only for a
  version the user should be able to compare against.
- In your reply, say that the page is open in the browser pane and give the
  file's path. Don't repeat the page's content in the terminal.

## Targets

Every action takes a **target**: either a ref (`e1`, `e2`, … from `snapshot`)
or a CSS selector (`#login`, `.btn`, `input[name=email]`). Refs go stale when
the page changes, so take a fresh `snapshot` after navigation.

```sh
"$SPLITGRID_BROWSER_CLI" open http://localhost:5173/login
"$SPLITGRID_BROWSER_CLI" wait selector "form"
"$SPLITGRID_BROWSER_CLI" snapshot              # -> refs for the inputs and buttons
"$SPLITGRID_BROWSER_CLI" fill e3 "user@example.com"
"$SPLITGRID_BROWSER_CLI" fill e4 "hunter2"
"$SPLITGRID_BROWSER_CLI" click e5
"$SPLITGRID_BROWSER_CLI" wait text "Welcome"
"$SPLITGRID_BROWSER_CLI" screenshot
```

## Commands

Run `"$SPLITGRID_BROWSER_CLI" help` for the authoritative list.

Navigate and inspect:
- `list`: browsers in this workspace and which one you own
- `open|navigate|goto <url|path>`: focus your browser (or create one in your zone) and load it
- `close [all]`: close your browser pane in your zone (`all` closes every one there)
- `pushstate <url>`: SPA navigation via `history.pushState`, no reload
- `back` · `forward` · `reload`
- `get url|title|text|html|console`
- `get text|html|value|attr|count|box <selector> [attr]`: read one element
  (`get value #email`, `get attr a.logo href`, `get count li.item`, `get box #app`)
- `is visible|enabled|checked <selector>`: boolean state
- `screenshot [--full]`: returns `{"screenshot":"<png path>"}`; `--full` captures the whole page
- `pdf`: export the page, returns `{"pdf":"<path>"}`
- `eval <js>`: run JS in the page, returns `{"result": ...}`

Interact:
- `snapshot [--viewport]`: tag interactive elements and return refs
- `click` · `dblclick` · `hover` · `focus <target>`
- `fill <target> <text>`: set an input or textarea value
- `type <text>`: type into the focused field
- `press <key> [target]`: a real keypress (`Enter`, `Tab`, `Escape`, `Control+a`, `ArrowDown`)
- `select <target> <value>`: choose a `<select>` option
- `check` · `uncheck <target>`
- `scroll up|down|left|right [px]` · `scrollintoview <target>`
- `find <text|role|label|placeholder|testid|selector> <query> <action> [value]`:
  locate one element and act on it (click, fill, check, uncheck, hover, focus,
  text), e.g. `find text "Sign in" click`, `find label Email fill me@x.com`

Wait:
- `wait load` · `wait selector <css>` · `wait text <substring>`
- `wait url <substring|regex>`: e.g. after a redirect
- `wait fn <js>`: until the expression is truthy (`wait fn "window.__ready===true"`)
- `wait <ms>`: fixed pause
- `--timeout <ms>` on the polling waits (default 10s, max 30s)

State and network:
- `storage local|session [get [key] | set <k> <v> | clear]`
- `cookies [list|clear]`
- `clipboard read|write [text]`
- `set viewport <w> <h>` · `set viewport reset`
- `network start` → reproduce → `network list` (statuses, types, failures) → `network stop` → `network clear`

## Several browsers and zones

The browser you `open` is **yours** (bound to this terminal and zone), and
commands without `--target` act on it. If you own more than one, the CLI
returns `ambiguous_target` with the list. Re-run with `--target <surface>`,
using the id from `open` or `list`.

A new browser pane opens in this agent's zone, the framed region its terminals
from `$SPLITGRID_TERMINAL_CLI` also live in. `--split right|down` and
`--at <pane>` place it, as with terminals. A **subagent passes
`--zone <name>`** on every call (the same short name each time), so it gets its
own browser and zone instead of sharing the parent's. Only panes in the zone
can be closed.

## Tabs in the user's real browser

When the user has the SplitGrid browser extension and has activated a tab in
it, `list` also shows that tab with a `tab:` id (`tab:c1.842`), `"kind":"tab"`
and `writable` (false means read-only, so `eval` and `open` are refused). Pass
it as `--target tab:c1.842`. Use it when the page needs the user's real
session, such as a logged-in app.

Supported on a tab: `open|navigate|goto`, `back`, `forward`, `reload`,
`get url|title|text|html`, `eval`, `screenshot [--full]`, and `close` (which
hands the tab back to the user). Every other command returns
`not_supported_on_tab`. Drop `--target` to run it in a browser pane instead.
