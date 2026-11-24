# SplitGrid Lite

A native macOS port of SplitGrid, the workspace terminal manager, written in Rust on
[gpui](https://github.com/zed-industries/zed/tree/main/crates/gpui) (through `gpui-kit`).
It reads and writes the same workspace files and data directory as the Electron app.

Licensed under the GNU AGPL v3 or later (see `LICENSE`), as SplitGrid is.

What it has:
- tiled workspaces with local and SSH terminals, focus mode and collapsible panes;
- browser, IDE, SQL and SFTP panes, plus workspace SFTP sync;
- terminals that outlive the app (a per-user PTY daemon), with a menu-bar list of sessions;
- agent integrations: the `sg-*` CLIs and skills for Claude Code, plus the browser extension.

macOS only.

## Build

Requirements:
- macOS 13 or later;
- the Rust toolchain pinned in `rust-toolchain.toml`.

```sh
cargo build                 # debug build of the app
cargo test --workspace      # unit tests
scripts/bundle.sh           # release build → target/release/SplitGrid Lite.app
```

`scripts/bundle.sh` signs with the first "Apple Development" identity in the keychain.
Set `SG_SIGN_IDENTITY` to a SHA-1 from `security find-identity -v -p codesigning` to pick
another, or to `-` for an ad-hoc signature. An ad-hoc build loses macOS privacy grants
on every rebuild.

The browser extension lives in `extension/`. Build it with `npm install && npm run build`,
then load `extension/dist` unpacked in Chrome.

## Layout

| Path | What |
| --- | --- |
| `src/` | the app: window, grid, panes, settings, agent CLIs |
| `crates/sg-ptyd` | persistent terminals: the PTY daemon, its protocol and client |
| `crates/sg-sessions` | the menu-bar helper listing persistent sessions |
| `crates/sg-history` | opt-in searchable history of Claude Code sessions |
| `crates/sql-*` | the SQL pane's drivers (PostgreSQL, MySQL, SQLite, SQL Server) and language tools |
| `extension/` | the Chrome extension agents drive a browser tab through |
| `resources/` | agent helper scripts, `sg-*` skills and notification sounds, copied from the Electron app and kept in step with it by hand |
| `assets/`, `icon/` | icons and the app icon |
| `scripts/` | bundling and icon generation |

The code is hand-formatted at about 160 columns. Don't run `cargo fmt` on whole files.
