# atrium-rs

A native macOS AI browser written in Rust. It started as a reverse-engineering
of [Aside](https://aside.com) and borrows ideas from Arc, Zen, Dia, Comet and
Safari 26. The built-in assistant runs on **opencode** through its server API
(`opencode serve`), so it uses whatever models you have configured there,
including the free OpenCode Zen models.

## Features

- **Native shell:** a real window with WKWebView tabs. The sidebar and
  assistant sit on macOS 26 Liquid Glass (`NSGlassEffectView`) over a vibrant
  window background. The app uses the system font and your system accent colour.
- **Sidebar (Arc):** back/forward/reload and the address pill, favorite tiles,
  vertical tabs, and **Spaces** at the bottom (⌃← / ⌃→).
- **Command bar (Arc), ⌘T / ⌘K / ⌘L:** search, enter a URL, switch tabs, search
  history and chats, run actions or ask the assistant. It floats over a frozen,
  dimmed snapshot of the page.
- **Split View (Zen/Arc), ⇧⌘= or ⌥-click a tab:** two tabs side by side.
- **Glance (Zen) / Peek (Arc):** ⌥-click any link to preview it in a floating
  card, then open it as a tab or in a split. ⌘-click opens a background tab.
- **Compact Mode (Zen), ⇧⌘S:** hides the sidebar until you hover the left edge.
- **Start page (Safari):** an address field with a Search/Ask toggle (Tab),
  Favorites, Frequently Visited and Recent Chats.
- **Assistant (Comet sidecar / Dia), ⌘E:**
  - Docks beside the page or opens as a full page.
  - It always sees the current tab. `@` adds other tabs as context, and `/`
    offers shortcuts (summarize, compare, extract, translate, research, reply).
  - While it drives the page, the page gets an accent outline and the element
    it clicks or types into is highlighted (Atlas-style).
  - Chats run in parallel as opencode sessions (Opera Neon-style tasks).

## How it works

```
 ┌──────────────────────── atrium (one process) ────────────────────────────┐
 │  tao window + muda menus                                                │
 │   ├─ NSGlassEffectView panes (positioned from the shell's layout)        │
 │   ├─ shell WKWebView (transparent): sidebar, start page, command bar,   │
 │   │    assistant, all served from 127.0.0.1:7777/shell                  │
 │   └─ one WKWebView per tab (+ Glance), laid over the shell's panes      │
 │  axum server :7777 ── /api/chat, /api/events (SSE), /api/tool ...       │
 │  opencode client ── prompt_async, /event, POST /mcp ─┐                  │
 └──────────────────────────────────────────────────────┼──────────────────┘
        opencode serve :4097 ── spawns `atrium mcp` ◄─────┘
        (stdio MCP bridge → /api/tool → browser tools on the active tab)
```

The browser tools follow Aside's agent design (reverse-engineered from Aside
1.0.1022 / agent 1.26.1024):

- **`snapshot`** produces a compact accessibility-style list of interactive
  elements with refs (`e12`).
- **Actions:** `click`, `type`, `select_option`, `press_key` and `scroll` target
  those refs.
- **Tabs and navigation:** `navigate`, `open_tab`, `switch_tab`, `close_tab`,
  `tabs`.
- **Reading:** `read_page`, `screenshot`, `evaluate`, `wait`.

## Usage

Requires macOS (26 for Liquid Glass; older versions fall back to vibrancy) and
[opencode](https://opencode.ai) with at least one provider.

```bash
cargo build --release
./target/release/atrium
```

Ask from the terminal while it runs:

```bash
./target/release/atrium ask --model opencode/big-pickle "summarize this page"
```

Options: `--port` (UI/API, default 7777), `--opencode-url` (use an existing
server), `--opencode-port` (4097). Data lives in `~/.atrium-rs`.
`POST /api/debug/shell` runs JS in the UI, for development; it listens on
127.0.0.1 only.
