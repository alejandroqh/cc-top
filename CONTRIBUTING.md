# Contributing

## Style guide

Follow this guide for any UI change. The constants live at the top of `src/main.rs`.

### Rules

- **Only explicit 24-bit colours.** Use the palette constants, never named ANSI colours
  (`Color::Green`, `.red()`, `.dark_gray()`) or `Color::Indexed`: those change with the terminal theme.
- **cc-top paints its own background** (`BG` on the main block). Never assume the terminal's.
  Widgets that draw on a canvas (`Chart`) reset cells to the terminal background unless they get
  `.bg(BG)` too.
- **Each colour has one meaning per area.** Check the table before reusing one, and add a constant
  rather than tweaking a hex inline.
- **New panels go through `panel(icon, name, extra)`.** It gives the rounded border, the notched title
  and 1 cell of padding left and right. A bottom-right note goes through `with_note`, the panel is
  drawn with `draw_panel`, and an empty state with `empty`. Rows start with `row_label`, and an idle
  session's rows go through `fade`.
- **Warnings go through `level(pct, normal, spin)`**, so every meter warns at the same thresholds.
- Assume a Nerd Font, but only use Nerd Font glyphs for decoration (icons). Data must stay readable without one.

### Palette

| Constant | Hex       | Used for                                                        |
| -------- | --------- | --------------------------------------------------------------- |
| `BG`     | `#1a1b26` | background of the whole app                                     |
| `TEXT`   | `#c0caf5` | primary text: titles, values, busy clock, rate-limit %, session name |
| `MUTED`  | `#737aa2` | secondary text: legend labels, session %, session info values, reset time, key hints · `in` / `out` labels and in-flight baseline (Tokens) |
| `FAINT`  | `#565f89` | low-priority text: version, `:`, "resets in", "up" / "since" / "tok", idle `·` / `00:00`, empty states |
| `TRACK`  | `#2c304a` | empty part of every bar                                         |
| `BORDER` | `#3b4261` | panel borders and their title notches                           |
| `FRAME`  | `#3d59a1` | main window border, scrollbar track                             |
| `ACCENT` | `#7aa2f7` | panel icons, count badge, key chips (`q`, `+-`, `↑↓`), scrollbar thumb · Repo box flash and `read` for a file read |
| `CYAN`   | `#7dcfff` | row labels: session number, `5h` / `7d`, core number, `M`, `↑` / `↓` |
| `TEAL`   | `#73daca` | heat gradient start (0%) · `opencode` tag (Sessions)             |
| `GREEN`  | `#9ece6a` | cache hit · CPU used · busy spinner · finished tool call `✓` · Repo box flash · `+N` added lines (Repo, Sessions) · heat gradient 50% |
| `YELLOW` | `#ebcb8b` | cache write · warning ≥ 80% · heat gradient 80%                 |
| `RED`    | `#f7768e` | uncached input · failed tool call `✗` · Repo box flash · `-N` removed lines (Repo, Sessions) · critical ≥ 90% · heat gradient 100% |
| `ORANGE` | `#ff9e64` | htop-style meter brackets `[ ]`                                 |
| `CLAUDE` | `#d97757` | the `✻` mark in the app title                                   |

### What the colours mean

**Session context bar.** It is stacked left to right in the order the API reports usage:

| Segment          | Colour   | Healthy looks like                                  |
| ---------------- | -------- | --------------------------------------------------- |
| cache hit (read) | `GREEN`  | most of the bar: the prefix is reused and cheap     |
| cache write      | `YELLOW` | a thin tip; a long one means the cache was missed   |
| uncached input   | `RED`    | ~0                                                  |

**Session info line (`session_info`).** Under each meter: the session's name, model and effort,
how long it has been up and when it started (local time), its tokens so far (counted like Tokens,
subagents included), and the lines it added and removed. Lines are Claude Code's own count, so
unlike Repo they include edits made through Bash. Model, effort and lines show once the first
status-line payload arrives. In a narrow column the name is cut with `…`, and the parts after it are
left off from the right. Opencode sessions have `opencode` before their model.

**System (`draw_system`).** One htop meter per core (`1: [|||  23.4%]`), `GREEN` for its load since
the last refresh, numbered and column-major like Sessions (`grid`). Under them, `M:` is memory used
of total, its label right-aligned with the core numbers, drawn like a rate limit (`heat_bar`). Its bar ends in the same column as theirs, so the
two panels line up. CPU text never warns, because a busy core is normal. Memory text goes through `level`.
The numbers come from the `sysinfo` crate, so the panel works on macOS and Linux.

**Network (`draw_network`).** Bytes sent (`↑`) and received (`↓`) per second, drawn as strips like
Tokens: `STRIP_ROWS` (2) rows tall, one sample per refresh and per cell. Only physical interfaces
count (`en*`, `eth*`, `wl*`). VPN tunnels and container bridges would count traffic twice, and
loopback never leaves the machine. Each strip has its own scale, shown in the note, because a
download would flatten the uploads next to it. Rates use `bytes()`: `512B`, `12.3K`, `4.1M`, in
powers of 1024 like htop.

**Heat bars (`heat_bar`).** Used for rate limits (% of the 5h / 7d window used) and for memory. Each filled cell is coloured by its
position on the bar (`heat`): teal, then green at 50%, yellow at 80%, red at 100%. A fuller bar
therefore shows warmer colours at its tip.

**Tokens histogram (`draw_tokens`, `token_strip`).** Two strips per session, stacked in session
order: `in`, what its API calls read (input + cache write + cache read), then `out`, what they wrote
(output, thinking included). Each column is a bucket of `ZOOM` seconds, newest at the right edge:
2 s to start with, and `+` / `-` step through 1 s, 2 s, 5 s, 10 s, 30 s and 1 m. Columns are
bucketed at every redraw, so the strips scroll on time even between refreshes. A session's tokens
include those of its subagents, which write transcripts of their own.

A call's tokens are only known once its response is written, so its columns fill in then. What it
read lands in the column its request went out in (the transcript's previous user line: the prompt
or the tool results). What it wrote is spread evenly from then to the response's last content block,
so a long answer shows as a band of its throughput instead of one spike. While a request is out,
the `out` baseline from the column it went out in to now shows in `MUTED` instead of `TRACK`, so
the strip moves while the model works; the band fills with output when the response lands. A
compaction logs no usage, so it counts as reading the conversation size (`compactMetadata.preTokens`)
when it finishes; the summary's output tokens are lost. The number after each strip is its tokens
in the last minute (`between`), so it doesn't drop to 0 between calls.

- A bucket is a solid bar of eighth blocks (`▁▂▃▄▅▆▇█`), 8 levels per row. Strips are
  `STRIP_ROWS` (2) rows tall, so 16 levels.
- Each cell is coloured by `heat` of the height it reaches against the scale, so a bar warms from
  teal at the bottom through green and yellow to red at the top. Light use stays teal, and the top
  block of a bar shows its exact height.
- `in` and `out` have a scale each, since output is about a hundred times smaller than what a call
  reads. Each is `nice_top` of the busiest visible bucket of every session's strips of that side,
  shown in the bottom-right note, and drops again once that burst scrolls off the left.
- Any use at all shows at least one level. Empty buckets show a `▁` baseline in `TRACK`.
- Idle sessions are dimmed, as in Sessions.

**Activity log (`draw_activity`).** The last `ACTIVITY` (5) tool calls of all sessions together,
newest on top. Each row shows the session number, then the call's state: a `GREEN` spinner while it
runs, a `GREEN` `✓` once it is done, or a `RED` `✗` if it failed or was rejected. After the tool
name comes what the call works on: the command, path, pattern or URL. It is collapsed onto one
line, with the session's cwd stripped and the home dir shown as `~`, and cut with `…` to fit. The
clock on the right ticks in `TEXT` bold while the call runs, then shows its duration in `FAINT`.
MCP tools show only their tool name (`mcp__server__tool` → `tool`). Calls made by one of the
session's subagents have `↳` after the session number instead of `:`.

**Repo grid (`draw_repo`).** One panel per session, in session order, with one rounded box per
top-level folder or file in its cwd; the title names it (`1: CCtop`). Folders come first, in `TEXT` bold with
a trailing `/`, then files in `MUTED`. Dotfiles and names listed in `.gitignore` are left out, and
names longer than `BOX_NAME` (16) are cut with `…`. Boxes flow left to right and wrap, and each
panel is as tall as its boxes.

Click a folder box to list what is inside it. A `../` box comes first to go back up, and the title
shows where you are (`1: CCtop/src`). In there, an edit elsewhere in the cwd flashes `../`. Each
panel remembers its own folder, and only flashes for its own session's reads and edits. Clicks need the mouse captured, so while cc-top runs, selecting text
in its terminal needs Shift (Option in iTerm2).

When an Edit or Write inside an entry goes through, its border turns `GREEN` if the edit added at least
as many lines as it removed, else `RED`. A new file counts as all added. A Read turns it `ACCENT`
blue, from the moment the call is made. The colour holds for half of
`FLASH_MS` (4 s), then fades back to `BORDER`; the background never changes. The bottom-right note shows the last one, edits git-style:
`src/main.rs +12 -3 · 5s ago`, or `src/main.rs read · 5s ago`. Only Read, Edit and Write count, from the
session and its subagents. Files read or changed through Bash (`cat`, `sed`, `rm`) or in your
editor don't flash.

**Scrolling.** Every panel takes the height it needs. When they don't all fit, the main window
scrolls: `↑` / `↓` a row, `PgUp` / `PgDn` a page, the mouse wheel 3 rows. Only then does a
scrollbar show over the main window's right border (an `ACCENT` `┃` thumb on a `FRAME` `│` track),
and `↑↓ scroll` joins the key hints. The main window's border and title stay put.

**Thresholds (`level`).** These apply to the percentage text of every meter except CPU:

| Usage   | Style                                                      |
| ------- | ---------------------------------------------------------- |
| < 80%   | the meter's normal text colour                             |
| ≥ 80%   | `YELLOW` bold                                              |
| ≥ 90%   | `RED`, pulsing between bold and dim every 500 ms           |

### Modifiers

| Modifier | Where                                                                             |
| -------- | --------------------------------------------------------------------------------- |
| bold     | titles, row labels, brackets, busy spinner and clock, badges, warnings            |
| dim      | whole row of an **idle** session, so the busy ones stand out. Warnings opt out (`not_dim`) |
| italic   | empty states ("no sessions", "no data yet") and the "updated … ago" note          |
| pulse    | critical (≥ 90%) values only                                                      |

cc-top draws the pulse itself, toggling with the 100 ms redraw tick. It doesn't use the terminal's
blink attribute, because many terminals ignore it (iTerm2 does by default).

### Glyphs

| Glyph              | Meaning                                                         |
| ------------------ | --------------------------------------------------------------- |
| `╭─╮ ╰─╯`          | rounded borders on the main window and every panel              |
| `─┤ title ├─`      | title notch, top-left for names, bottom-right for legends/notes |
| `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`       | busy spinner (one frame per 100 ms), also a running tool call   |
| `✓` `✗`            | finished / failed tool call                                     |
| `↳`                | Activity: the call was made by a subagent of that session       |
| `·`                | idle marker                                                     |
| `\|`               | session context bar (htop CPU-meter style), track in `TRACK`    |
| `━` `╸` `╺`        | heat bar: full cell, half-cell tip, half-cell gap before the track |
| `▁▂▃▄▅▆▇█`         | Tokens and Network histograms: a bucket per cell, filled bottom-up |
| `▁` in `TRACK`     | histogram baseline where a bucket is empty                      |
| `▁` in `MUTED`     | Tokens `out` baseline while a request is in flight              |
| `■`                | legend swatch                                                   |
| `┃` on `│`         | scrollbar thumb on its track, over the main window's right border |
| `✻`                | app mark                                                        |
| `\u{f2db}`         | Nerd Font microchip icon, System panel                          |
| `\u{f0ec}`         | Nerd Font exchange icon, Network panel                          |
| `\u{f120}`         | Nerd Font terminal icon, Sessions panel                         |
| `\u{f0e4}`         | Nerd Font tachometer icon, Rate limits panel                    |
| `\u{f06d}`         | Nerd Font fire icon, Tokens panel                               |
| `\u{f1da}`         | Nerd Font history icon, Activity panel                          |
| `\u{f07b}`         | Nerd Font folder icon, Repo panel                               |

### Row layouts

```
1: [⠋ 09:04][||||||||||||||||||||                     17.0%]
│   │ │      │                                        └ % of the context window, via level()
│   │ │      └ context bar: GREEN | YELLOW | RED segments over a TRACK
│   │ └ busy time MM:SS, or " HhMM" past an hour (always 5 cells); 00:00 while idle
│   └ spinner while busy, · while idle
└ session number (CYAN bold), sessions numbered by start time

1: [⠋ 09:04][||||||||||||||||||||                     17.0%]
   Cc-top Rust project · Opus 5.5 xhigh · up 4h 19m since 13:29 · 41.2M tok · +949 -254
   │                     │                │               │       │           └ GREEN +added, RED -removed
   │                     │                │               │       └ tokens so far, via si()
   │                     │                │               └ local start time
   │                     │                └ uptime, via human()
   │                     └ model and effort (MUTED)
   └ session name (TEXT bold); the whole line dims with the meter while idle

5h ━━━━━━━━━━━━━━╸━━━━━━━━━━━━━━━━━━━━━  44.0%  resets in 2h 13m
   └ heat-coloured fill, then the track   └ level()   └ FAINT label, MUTED value

1: in           ▆         ▇    ▇   735.5K   ← tokens read in the last minute (si), FAINT at 0
       ▁▁▁▁▁▁▁▁▁█▁▁▁▁▁▁▁▁▁█▁▁▁▁█▁▁   ← a spike in the column each request went out in
   out          ▃▃▃▂     ▄▄▄             3.8K   ← tokens written in the last minute
       ▁▁▁▁▁▁▁▁▁████▇▁▁▁▁▁███▁▁▁▁▁▁   ← output spread over each call; ▁ in MUTED while one is out
│  │   └ oldest on the left, now on the right; ▁ in TRACK where empty; heat-coloured by height
│  └ in / out (MUTED)
└ session number (CYAN bold), then `:` (FAINT)

1: ⠋ Bash cargo test 2>&1 | tail -5                 00:12
│  │ │    │                                         └ running time, or duration once done
│  │ │    └ what it works on (MUTED), cut with … to fit
│  │ └ tool name (TEXT bold), padded to the longest one shown
│  └ spinner while running, ✓ done, ✗ failed
└ session number (CYAN bold), then `:`, or `↳` for a subagent's call; newest call on top

╭──────╮ ╭───────────╮ ╭────────────╮
│ src/ │ │ README.md │ │ Cargo.toml │   ← 3 cells tall, label + 4 wide, 1 cell apart
╰──────╯ ╰───────────╯ ╰────────────╯
  └ BORDER on BG; after an edit the border turns GREEN / RED, after a read ACCENT, then fades back
```

Token counts use `si()`: `950`, `12.3K`, `4.1M`. Panel order, top to bottom: System, Network, Rate
limits, Sessions, Tokens, Activity, Repo. Sessions, Tokens, Activity and Repo share the same session numbering.
