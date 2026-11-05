mod claude;
mod machine;
mod opencode;

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::stdout;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use claude::{ACTIVITY, Limit, Quota, Session, Tool};
use machine::Machine;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
    MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Widget,
};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const ROWS_PER_COL: usize = 4; // like htop: start a new column every 4 meters
const MIN_METER_W: u16 = 32;
const REFRESH: Duration = Duration::from_secs(1); // how often the data is read again
const TICK: Duration = Duration::from_millis(100); // how often the screen is drawn again
const ZOOM: [u64; 6] = [1, 2, 5, 10, 30, 60]; // seconds per column in Tokens, picked with + / -
const ZOOM_START: usize = 1; // 2s: moves often enough to look live, and shows the last few minutes
const HISTORY: usize = 600; // Network samples kept, enough for a 600-cell wide strip
const STRIP_ROWS: usize = 2; // height of each Tokens and Network histogram
const FLASH_MS: u64 = 4_000; // how long a Repo box flashes after an edit inside it
const BOX_NAME: usize = 16; // longest name a Repo box shows before cutting it with …

// Explicit 24-bit palette, so cc-top looks the same whatever the terminal's 16 colours are.
const BG: Color = Color::Rgb(0x1a, 0x1b, 0x26);
const TEXT: Color = Color::Rgb(0xc0, 0xca, 0xf5);
const MUTED: Color = Color::Rgb(0x73, 0x7a, 0xa2); // secondary text
const FAINT: Color = Color::Rgb(0x56, 0x5f, 0x89); // idle markers, labels
const TRACK: Color = Color::Rgb(0x2c, 0x30, 0x4a); // empty part of a bar
const BORDER: Color = Color::Rgb(0x3b, 0x42, 0x61); // panel borders
const FRAME: Color = Color::Rgb(0x3d, 0x59, 0xa1); // main window border
const ACCENT: Color = Color::Rgb(0x7a, 0xa2, 0xf7);
const CYAN: Color = Color::Rgb(0x7d, 0xcf, 0xff);
const TEAL: Color = Color::Rgb(0x73, 0xda, 0xca);
const GREEN: Color = Color::Rgb(0x9e, 0xce, 0x6a);
const YELLOW: Color = Color::Rgb(0xeb, 0xcb, 0x8b);
const RED: Color = Color::Rgb(0xf7, 0x76, 0x8e);
const ORANGE: Color = Color::Rgb(0xff, 0x9e, 0x64); // htop-style meter brackets
const CLAUDE: Color = Color::Rgb(0xd9, 0x77, 0x57);

// Nerd Font glyphs.
const ICON_SYSTEM: &str = "\u{f2db}"; // microchip
const ICON_NETWORK: &str = "\u{f0ec}"; // exchange
const ICON_SESSIONS: &str = "\u{f120}"; // terminal
const ICON_LIMITS: &str = "\u{f0e4}"; // tachometer
const ICON_TOKENS: &str = "\u{f06d}"; // fire
const ICON_ACTIVITY: &str = "\u{f1da}"; // history
const ICON_REPO: &str = "\u{f07b}"; // folder

fn main() -> std::io::Result<()> {
    match std::env::args().nth(1).as_deref() {
        None => {}
        Some("statusline") => return claude::save_statusline(),
        Some(flag @ ("--setup" | "--uninstall")) => {
            let runs: [fn() -> Result<(), String>; 2] = if flag == "--setup" {
                [claude::setup, opencode::setup]
            } else {
                [opencode::uninstall, claude::uninstall]
            };
            let mut failed = false;
            for run in runs {
                if let Err(e) = run() {
                    eprintln!("cc-top {flag}: {e}");
                    failed = true;
                }
            }
            std::process::exit(failed as i32);
        }
        Some(_) => {
            eprintln!("usage: cc-top [--setup | --uninstall]");
            std::process::exit(2);
        }
    }

    let dir = claude::config_dir();
    let (mut sessions, mut quota) = (Vec::new(), None);
    let mut transcripts = claude::Transcripts::default();
    let mut machine = Machine::new();
    let mut collected: Option<Instant> = None;
    // The folder clicked into in each session's Repo (by session id), each session's folder and
    // what it holds, and where each folder box was drawn.
    let mut browse: HashMap<String, PathBuf> = HashMap::new();
    let (mut listings, mut hits) = (Vec::new(), Vec::new());
    // How far the page is scrolled, and where it shows.
    let (mut scroll, mut view) = (0u16, Rect::default());
    let mut zoom = ZOOM_START; // index into ZOOM
    let start = Instant::now();
    let mut next_frame = start;
    // Clicks open folders in Repo. While the mouse is captured, selecting text in the terminal
    // needs Shift (Option in iTerm2). A panic must not leave the terminal reporting the mouse.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture);
        hook(info);
    }));
    let result = ratatui::run(|terminal| {
        execute!(stdout(), EnableMouseCapture)?;
        loop {
            if collected.is_none_or(|t| t.elapsed() >= REFRESH) {
                sessions = claude::sessions(&dir, &mut transcripts, unix_ms());
                sessions.extend(opencode::sessions(&dir));
                machine.refresh(HISTORY);
                quota = claude::quota(&dir);
                browse.retain(|id, _| sessions.iter().any(|s| &s.id == id)); // ended
                listings = sessions
                    .iter()
                    .map(|s| {
                        let at = repo_dir(s, browse.get(&s.id).map(PathBuf::as_path));
                        let up = (at != s.cwd).then(|| ("..".to_string(), true));
                        let entries = up.into_iter().chain(entries(&at)).collect();
                        (at, entries)
                    })
                    .collect();
                collected = Some(Instant::now());
            }
            if Instant::now() >= next_frame {
                let spin = (start.elapsed().as_millis() / TICK.as_millis()) as usize;
                terminal.draw(|frame| {
                    (view, hits) = draw(
                        frame,
                        &machine,
                        &sessions,
                        quota.as_ref(),
                        &listings,
                        (spin, ZOOM[zoom]),
                        &mut scroll,
                    );
                })?;
                next_frame = Instant::now() + TICK;
            }
            if !event::poll(next_frame.saturating_duration_since(Instant::now()))? {
                continue;
            }
            // Only what a key or click changes is drawn right away: the mouse moving over the
            // window (reported too while it is captured) waits for the next frame.
            let before = (scroll, zoom, collected);
            // `draw` clamps the scroll to the page.
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Up => scroll = scroll.saturating_sub(1),
                    KeyCode::Down => scroll = scroll.saturating_add(1),
                    KeyCode::PageUp => scroll = scroll.saturating_sub(view.height),
                    KeyCode::PageDown => scroll = scroll.saturating_add(view.height),
                    KeyCode::Char('+' | '=') => zoom = zoom.saturating_sub(1),
                    KeyCode::Char('-') => zoom = (zoom + 1).min(ZOOM.len() - 1),
                    _ => {}
                },
                Event::Mouse(m) => match m.kind {
                    MouseEventKind::ScrollUp => scroll = scroll.saturating_sub(3),
                    MouseEventKind::ScrollDown => scroll = scroll.saturating_add(3),
                    MouseEventKind::Down(MouseButton::Left) => {
                        let at = Position::new(m.column, m.row);
                        let on_page = view
                            .contains(at)
                            .then(|| Position::new(at.x - view.x, at.y - view.y + scroll));
                        if let Some(p) = on_page
                            && let Some((_, i, folder)) = hits.iter().find(|(r, ..)| r.contains(p))
                        {
                            browse.insert(sessions[*i].id.clone(), folder.clone());
                            collected = None; // list it right away
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
            if (scroll, zoom, collected) != before {
                next_frame = Instant::now();
            }
        }
    });
    execute!(stdout(), DisableMouseCapture)?;
    result
}

/// Draws everything; `listings` has each session's Repo folder and what it holds. The panels go on
/// a page as tall as they need, and the main window shows it from row `scroll` on, which this
/// clamps. `spin` counts redraw ticks, and `secs` is a Tokens column. Returns where the page shows,
/// and where on the page each folder box in Repo is, the session it belongs to and the folder it opens.
fn draw(
    frame: &mut Frame,
    machine: &Machine,
    sessions: &[Session],
    quota: Option<&Quota>,
    listings: &[(PathBuf, Vec<(String, bool)>)],
    (spin, secs): (usize, u64),
    scroll: &mut u16,
) -> (Rect, Vec<(Rect, usize, PathBuf)>) {
    let area = frame.area();
    let view = area.inner(Margin::new(1, 1)); // inside the main border
    let width = view.width.saturating_sub(4); // panel borders + padding
    let now = unix_ms(); // one clock for the whole frame
    let (cols, rows) = grid(sessions.len(), width);
    let cpu_grid = grid(machine.cpus.len(), width);
    let boxes: Vec<_> = listings
        .iter()
        .map(|(_, entries)| repo_boxes(entries, width))
        .collect();
    let mut heights = vec![
        cpu_grid.1 as u16 + 1 + 2, // the cores, then memory
        (2 * STRIP_ROWS) as u16 + 2,
        4,
        rows.max(1) as u16 * 2 + 2, // the meter, then the info line
        (sessions.len().max(1) * 2 * STRIP_ROWS) as u16 + 2, // in, then out
        ACTIVITY as u16 + 2,
    ];
    heights.extend(
        boxes
            .iter()
            .map(|b| b.last().map_or(3, |(_, r)| r.bottom()) + 2),
    );
    if sessions.is_empty() {
        heights.push(3); // Repo, empty
    }
    let height: u16 = heights.iter().sum();
    let max = height.saturating_sub(view.height);
    *scroll = (*scroll).min(max);

    let title = vec![
        "✻ ".fg(CLAUDE).bold(),
        "cc-top".fg(TEXT).bold(),
        concat!(" v", env!("CARGO_PKG_VERSION")).fg(FAINT),
    ];
    let mut keys = vec![
        " q ".fg(BG).bg(ACCENT).bold(),
        " quit".fg(MUTED),
        "  ".into(),
        " +- ".fg(BG).bg(ACCENT).bold(),
        " zoom".fg(MUTED),
    ];
    if max > 0 {
        keys.extend([
            "  ".into(),
            " ↑↓ ".fg(BG).bg(ACCENT).bold(),
            " scroll".fg(MUTED),
        ]);
    }
    let main = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(FRAME)
        .style(Style::new().fg(TEXT).bg(BG)) // paint our own background too
        .title(notch(title, FRAME))
        .title_bottom(notch(keys, FRAME));
    frame.render_widget(main, area);
    if max > 0 {
        // Over the right border, between the corners.
        let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .track_style(FRAME)
            .thumb_symbol("┃")
            .thumb_style(ACCENT);
        let mut state = ScrollbarState::new(max as usize + 1)
            .position(*scroll as usize)
            .viewport_content_length(view.height as usize);
        frame.render_stateful_widget(bar, area.inner(Margin::new(0, 1)), &mut state);
    }

    let page = Rect::new(0, 0, view.width, height.max(view.height));
    let mut buf = Buffer::empty(page);
    buf.set_style(page, Style::new().fg(TEXT).bg(BG));
    let mut constraints: Vec<_> = heights.into_iter().map(Constraint::Length).collect();
    constraints.push(Constraint::Fill(1)); // else the last panel stretches to the bottom
    let areas = Layout::vertical(constraints).split(page);
    draw_system(&mut buf, areas[0], machine, cpu_grid, spin);
    draw_network(&mut buf, areas[1], machine);
    draw_quota(&mut buf, areas[2], quota, now, spin);
    draw_sessions(&mut buf, areas[3], sessions, (cols, rows), now, spin);
    draw_tokens(&mut buf, areas[4], sessions, secs, now);
    draw_activity(&mut buf, areas[5], sessions, now, spin);
    let repos = &areas[6..];
    if sessions.is_empty() {
        let body = draw_panel(&mut buf, panel(ICON_REPO, "Repo", vec![]), repos[0]);
        empty(&mut buf, body, "no sessions");
    }
    let mut hits = vec![];
    for (i, (s, (at, entries))) in sessions.iter().zip(listings).enumerate() {
        let repo = (i, s, at.as_path());
        let drawn = draw_repo(&mut buf, repos[i], repo, entries, &boxes[i], now);
        hits.extend(drawn.into_iter().map(|(rect, folder)| (rect, i, folder)));
    }
    for y in 0..view.height {
        for x in 0..view.width {
            frame.buffer_mut()[(view.x + x, view.y + y)] = buf[(x, y + *scroll)].clone();
        }
    }
    (view, hits)
}

/// The folder the Repo panel lists: the one clicked into, while it is inside the session's cwd.
fn repo_dir(s: &Session, browse: Option<&Path>) -> PathBuf {
    let inside = browse.filter(|b| b.starts_with(&s.cwd));
    inside.unwrap_or(&s.cwd).to_path_buf()
}

/// The folders (true) and files directly in `dir`, folders first, then by name. Leaves out dotfiles and
/// what `.gitignore` names. ponytail: exact names only (`/target`, `dist/`), no glob patterns.
fn entries(dir: &Path) -> Vec<(String, bool)> {
    let gitignore = fs::read_to_string(dir.join(".gitignore")).unwrap_or_default();
    let ignored: HashSet<&str> = gitignore
        .lines()
        .map(|l| l.trim().trim_start_matches('/').trim_end_matches('/'))
        .collect();
    let mut found: Vec<(String, bool)> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            Some((
                e.file_name().into_string().ok()?,
                e.file_type().ok()?.is_dir(),
            ))
        })
        .filter(|(name, _)| !name.starts_with('.') && !ignored.contains(name.as_str()))
        .collect();
    found.sort_by_cached_key(|(name, dir)| (!dir, name.to_lowercase()));
    found
}

/// A box per folder (true) or file in `entries`, flowing left to right in `width` cells and
/// wrapping: its label, and where it goes. Each box is 3 cells tall and 4 wider than its label.
fn repo_boxes(entries: &[(String, bool)], width: u16) -> Vec<(String, Rect)> {
    let (mut x, mut y) = (0, 0);
    entries
        .iter()
        .map(|(name, dir)| {
            let label = cut(
                &if *dir {
                    format!("{name}/")
                } else {
                    name.clone()
                },
                BOX_NAME,
            );
            let w = label.chars().count() as u16 + 4;
            if x > 0 && x + w > width {
                (x, y) = (0, y + 3);
            }
            let rect = Rect::new(x, y, w, 3);
            x += w + 1;
            (label, rect)
        })
        .collect()
}

/// The folders and files of one folder `at` in a session's cwd, a box each, after a `..` box
/// below the cwd. A box flashes when an edit inside it goes through: green when it added at least
/// as many lines as it removed, else red. Returns each folder box and the folder it opens.
fn draw_repo(
    buf: &mut Buffer,
    area: Rect,
    (i, s, at): (usize, &Session, &Path),
    entries: &[(String, bool)],
    boxes: &[(String, Rect)],
    now: u64,
) -> Vec<(Rect, PathBuf)> {
    let mut name = s
        .cwd
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    if let Ok(below) = at.strip_prefix(&s.cwd)
        && !below.as_os_str().is_empty()
    {
        name = format!("{name}/{}", below.display());
    }
    let mut title = vec![" · ".fg(FAINT)];
    title.extend(row_label(i + 1, 0));
    title.push(name.fg(TEXT));
    let mut panel = panel(ICON_REPO, "Repo", title);
    if let Some(c) = s.changes.last() {
        let path = c.path.strip_prefix(&s.cwd).unwrap_or(&c.path);
        let age = (now / 1_000).saturating_sub(c.at);
        let mut note = vec![path.display().to_string().fg(MUTED)];
        if c.read {
            note.push(" read".fg(ACCENT));
        } else {
            note.extend([
                format!(" +{}", c.added).fg(GREEN),
                format!(" -{}", c.removed).fg(RED),
            ]);
        }
        note.push(format!(" · {} ago", human(age)).fg(FAINT).italic());
        panel = with_note(panel, note);
    }
    let body = draw_panel(buf, panel, area);
    if entries.is_empty() {
        empty(buf, body, "empty folder");
        return vec![];
    }

    let mut hits = vec![];
    for ((label, r), (name, dir)) in boxes.iter().zip(entries) {
        let rect = Rect {
            x: body.x + r.x,
            y: body.y + r.y,
            ..*r
        }
        .intersection(body);
        if *dir {
            let folder = match name.as_str() {
                ".." => at.parent().unwrap_or(at).to_path_buf(),
                _ => at.join(name),
            };
            hits.push((rect, folder));
        }
        // The newest read or edit inside this entry, while it still flashes. `..` stands for the rest of
        // the cwd, outside the folder shown.
        let inside = |c: &claude::Change| match c.path.strip_prefix(at) {
            Ok(below) => below
                .components()
                .next()
                .is_some_and(|f| f.as_os_str() == name.as_str()),
            Err(_) => name == ".." && c.path.starts_with(&s.cwd),
        };
        // Changes are oldest first, so most frames, when none still flashes, compare no paths.
        let flash = s
            .changes
            .iter()
            .rev()
            .map(|c| (c, now.saturating_sub(c.at * 1_000)))
            .take_while(|&(_, age)| age < FLASH_MS)
            .find(|&(c, _)| inside(c));
        // The border takes the colour for the first half, then fades back to BORDER.
        let border = flash.map_or(BORDER, |(c, age)| {
            let k = ((FLASH_MS - age) as f64 / (FLASH_MS / 2) as f64).min(1.0);
            let colour = match c {
                c if c.read => ACCENT,
                c if c.added >= c.removed => GREEN,
                _ => RED,
            };
            mix(BORDER, colour, k)
        });
        let text = if *dir {
            label.clone().fg(TEXT).bold()
        } else {
            label.clone().fg(MUTED)
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(border)
            .padding(Padding::horizontal(1));
        Paragraph::new(text).block(block).render(rect, buf);
    }
    hits
}

/// `s` cut to `w` characters, ending in … when it was longer.
fn cut(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        return s.to_string();
    }
    let kept = s.chars().take(w.saturating_sub(1));
    kept.chain((w > 0).then_some('…')).collect()
}

/// The last ACTIVITY tool calls of all sessions together, newest on top.
fn draw_activity(buf: &mut Buffer, area: Rect, sessions: &[Session], now: u64, spin: usize) {
    let extra = vec![format!(" · last {ACTIVITY}").fg(FAINT)];
    let body = draw_panel(buf, panel(ICON_ACTIVITY, "Activity", extra), area);
    let mut log: Vec<(usize, &Tool)> = sessions
        .iter()
        .enumerate()
        .flat_map(|(i, s)| s.tools.iter().rev().map(move |t| (i + 1, t)))
        .collect();
    log.sort_by_key(|&(_, t)| Reverse(t.at)); // stable: calls from the same second stay newest first
    log.truncate(ACTIVITY);
    if log.is_empty() {
        empty(buf, body, "no tool calls yet");
        return;
    }
    let label_w = sessions.len().to_string().len();
    let name_w = log
        .iter()
        .map(|(_, t)| t.name.chars().count())
        .max()
        .unwrap_or(0);
    let (width, now) = (body.width as usize, now / 1000);
    let lines: Vec<Line> = log
        .iter()
        .map(|&(n, t)| activity_row(n, (label_w, name_w), t, width, now, spin))
        .collect();
    Paragraph::new(lines).render(body, buf);
}

/// `1: ⠋ Bash cargo test 2>&1 | tail -5      00:12`, the clock is how long the call ran, or has
/// been running so far. `widths` are of the session number and the tool name columns.
fn activity_row(
    n: usize,
    (label_w, name_w): (usize, usize),
    t: &Tool,
    width: usize,
    now: u64,
    spin: usize,
) -> Line<'static> {
    let (icon, end, clock_style) = match t.done {
        None => (spinner(spin), now, Style::new().fg(TEXT).bold()),
        Some((end, false)) => ("✓".fg(GREEN), end, Style::new().fg(FAINT)),
        Some((end, true)) => ("✗".fg(RED).bold(), end, Style::new().fg(FAINT)),
    };
    let arg_w = width.saturating_sub(label_w + name_w + 11); // ": ", icon, spaces, clock
    let arg = cut(&t.arg, arg_w);
    let [n, colon] = row_label(n, label_w);
    Line::from(vec![
        n,
        if t.sub { "↳ ".fg(FAINT) } else { colon }, // ↳ a subagent's call
        icon,
        format!(" {:<name_w$} ", t.name).fg(TEXT).bold(),
        format!("{arg:<arg_w$} ").fg(MUTED),
        Span::styled(clock(end.saturating_sub(t.at)), clock_style),
    ])
}

/// What each session read (in) and wrote (out) per column of `secs`, up to `now` (unix ms): two
/// block histograms per session, one session under the other. Out is about a hundred times smaller
/// than in, so each has its own scale, shared by every session so their heights compare.
fn draw_tokens(buf: &mut Buffer, area: Rect, sessions: &[Session], secs: u64, now: u64) {
    let label_w = sessions.len().to_string().len();
    let panel = panel(ICON_TOKENS, "Tokens", vec![" · last minute".fg(FAINT)]);
    // A column per cell; the rest of the row is the label (`1: out `) and the last minute's count.
    let cells = (panel.inner(area).width as usize).saturating_sub(label_w + 6 + VALUE_W);
    let strips: Vec<_> = sessions
        .iter()
        .map(|s| claude::buckets(&s.calls, now, secs, cells))
        .collect();
    let top = |out: bool, floor: f64| {
        let side = strips
            .iter()
            .flat_map(|(ins, outs)| if out { outs } else { ins });
        nice_top(side.copied().max().unwrap_or(0) as f64, floor)
    };
    // A few output tokens a second fill a column of out, so it can start lower.
    let (top_in, top_out) = (top(false, 1_000.0), top(true, 100.0));
    let note = vec![
        format!(
            "1 column = {} · top in {} out {}",
            human(secs),
            si(top_in),
            si(top_out)
        )
        .fg(FAINT)
        .italic(),
    ];
    let body = draw_panel(buf, with_note(panel, note), area);
    if sessions.is_empty() {
        empty(buf, body, "no sessions");
        return;
    }
    let span = secs * 1000;
    let lines: Vec<Line> = sessions
        .iter()
        .zip(&strips)
        .enumerate()
        .flat_map(|(i, (s, (ins, outs)))| {
            let (read, wrote) = claude::between(&s.calls, now.saturating_sub(60_000), now + 1);
            // The columns from the one the request in flight went out in, to now.
            let pending = s.pending.map_or(0, |sent| {
                (now / span).saturating_sub(sent / span) as usize + 1
            });
            let [n, colon] = row_label(i + 1, label_w);
            let blank = " ".repeat(label_w + 2).into();
            [
                (vec![n, colon, "in  ".fg(MUTED)], ins, top_in, read, 0),
                (vec![blank, "out ".fg(MUTED)], outs, top_out, wrote, pending),
            ]
            .into_iter()
            .flat_map(move |(label, strip, top, total, pending)| {
                token_strip(
                    &label,
                    strip,
                    top,
                    cells,
                    STRIP_ROWS,
                    count(total, si),
                    pending,
                )
            })
            .map(move |l| fade(l, s))
        })
        .collect();
    Paragraph::new(lines).render(body, buf);
}

const VALUE_W: usize = 7; // " 178.0K"

/// The number after a strip, `VALUE_W` cells, as `fmt` writes it. FAINT when there is nothing to count.
fn count(n: u64, fmt: fn(f64) -> String) -> Span<'static> {
    let w = VALUE_W - 1;
    format!(" {:>w$}", fmt(n as f64)).fg(if n > 0 { TEXT } else { FAINT })
}

/// One histogram, `rows` cells tall and `cells` wide, newest bucket at the right edge, after `label`.
/// A bucket is a solid bar of eighth blocks, so each row holds 8 levels. Each cell is coloured by
/// the height it reaches (`heat`): bars warm from teal at the bottom to red at the top of the scale.
/// `value` follows the first row. The last `pending` columns wait on a response: where they are
/// empty, their baseline shows in MUTED instead of TRACK.
fn token_strip(
    label: &[Span<'static>],
    buckets: &[u64],
    scale: f64,
    cells: usize,
    rows: usize,
    value: Span<'static>,
    pending: usize,
) -> Vec<Line<'static>> {
    const BLOCKS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    let shown = &buckets[buckets.len().saturating_sub(cells)..];
    let pad = cells - shown.len(); // short history: empty on the left
    let levels = rows * 8;
    // Any use at all shows at least one level.
    let height = |v: u64| match v {
        0 => 0,
        _ => ((v as f64 / scale * levels as f64).round() as usize).clamp(1, levels),
    };
    let blank = " ".repeat(label.iter().map(Span::width).sum());
    (0..rows)
        .map(|r| {
            let below = (rows - 1 - r) * 8; // levels under this row
            let mut spans = if r == 0 {
                label.to_vec()
            } else {
                vec![blank.clone().into()]
            };
            for c in 0..cells {
                let v = c.checked_sub(pad).map_or(0, |i| shown[i]);
                let fill = height(v).saturating_sub(below).min(8);
                spans.push(match (fill, r + 1 == rows) {
                    (0, true) if c + pending >= cells => "▁".fg(MUTED), // waiting on a response
                    (0, true) => "▁".fg(TRACK), // baseline where nothing happened
                    (0, false) => " ".into(),
                    _ => BLOCKS[fill].fg(heat((below + fill) as f64 / levels as f64)),
                });
            }
            if r == 0 {
                spans.push(value.clone());
            }
            Line::from(spans)
        })
        .collect()
}

/// Top of the y axis: the next round number above `max`, and at least `floor`, leaving headroom so
/// the highest line doesn't run along the border. Its half prints clean too (0 / 6.0M / 12.0M).
fn nice_top(max: f64, floor: f64) -> f64 {
    let max = max.max(floor) * 1.05;
    let mag = 10f64.powf(max.log10().floor());
    [
        1.0, 1.2, 1.4, 1.6, 1.8, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0,
    ]
    .into_iter()
    .map(|m| m * mag)
    .find(|&t| t >= max)
    .unwrap_or(10.0 * mag)
}

/// Token counts the way people say them: 950, 12.3K, 4.1M.
fn si(n: f64) -> String {
    match n {
        ..1e3 => format!("{n:.0}"),
        ..1e6 => format!("{:.1}K", n / 1e3),
        ..1e9 => format!("{:.1}M", n / 1e6),
        _ => format!("{:.1}B", n / 1e9),
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn draw_sessions(
    buf: &mut Buffer,
    area: Rect,
    sessions: &[Session],
    (cols, rows): (usize, usize),
    now: u64,
    spin: usize,
) {
    let n = sessions.len();
    let legend = vec![
        "■".fg(GREEN),
        " cache hit  ".fg(MUTED),
        "■".fg(YELLOW),
        " cache write  ".fg(MUTED),
        "■".fg(RED),
        " uncached".fg(MUTED),
    ];
    let badge = vec![" ".into(), format!(" {n} ").fg(BG).bg(ACCENT).bold()];
    let panel = with_note(panel(ICON_SESSIONS, "Sessions", badge), legend);
    let body = draw_panel(buf, panel, area);
    if n == 0 {
        empty(buf, body, "no sessions");
        return;
    }
    let label_w = n.to_string().len();
    draw_columns(buf, body, sessions, cols, rows, |n, s, width| {
        let row = session_row(n, label_w, s, width, spin, now);
        let info = session_info(label_w, s, width, now / 1000);
        vec![fade(row, s), fade(info, s)]
    });
}

/// One row per item, column-major like htop: 1-4 down the first column, 5-8 down the second.
/// `row` gets the item number, the item and the column width, and gives its lines. Needs `rows > 0`.
fn draw_columns<T>(
    buf: &mut Buffer,
    body: Rect,
    items: &[T],
    cols: usize,
    rows: usize,
    row: impl Fn(usize, &T, usize) -> Vec<Line<'static>>,
) {
    let areas = Layout::horizontal(vec![Constraint::Fill(1); cols])
        .spacing(2)
        .split(body);
    for (c, (chunk, area)) in items.chunks(rows).zip(areas.iter()).enumerate() {
        let lines: Vec<Line> = chunk
            .iter()
            .enumerate()
            .flat_map(|(r, s)| row(c * rows + r + 1, s, area.width as usize))
            .collect();
        Paragraph::new(lines).render(*area, buf);
    }
}

/// Every panel: rounded border, an icon + name title, 1 cell of padding left and right.
fn panel(icon: &'static str, name: &'static str, extra: Vec<Span<'static>>) -> Block<'static> {
    let mut title = vec![icon.fg(ACCENT), " ".into(), name.fg(TEXT).bold()];
    title.extend(extra);
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(BORDER)
        .title(notch(title, BORDER))
        .padding(Padding::horizontal(1))
}

/// `panel` with `note` set into the right of its bottom border.
fn with_note(panel: Block<'static>, note: Vec<Span<'static>>) -> Block<'static> {
    panel.title_bottom(notch(note, BORDER).right_aligned())
}

/// Draws `panel` over `area` and returns the area inside it.
fn draw_panel(buf: &mut Buffer, panel: Block<'static>, area: Rect) -> Rect {
    let body = panel.inner(area);
    panel.render(area, buf);
    body
}

/// What a panel says when it has nothing to show.
fn empty(buf: &mut Buffer, body: Rect, text: &'static str) {
    text.fg(FAINT).italic().render(body, buf);
}

/// `12: `, the label a row starts with: its number (or letter), right-aligned in `w` cells.
fn row_label(n: impl std::fmt::Display, w: usize) -> [Span<'static>; 2] {
    [format!("{n:>w$}").fg(CYAN).bold(), ": ".fg(FAINT)]
}

/// The busy spinner, at redraw tick `spin`.
fn spinner(spin: usize) -> Span<'static> {
    SPINNER[spin % SPINNER.len()].fg(GREEN).bold()
}

/// `line` dimmed while `s` is idle, so the busy sessions stand out.
fn fade(line: Line<'static>, s: &Session) -> Line<'static> {
    if s.busy_since.is_none() {
        line.dim()
    } else {
        line
    }
}

/// `─┤ content ├─`: a title set into a notch of its border.
fn notch(content: Vec<Span<'static>>, border: Color) -> Line<'static> {
    let mut spans = vec!["─┤ ".fg(border)];
    spans.extend(content);
    spans.push(" ├─".fg(border));
    Line::from(spans)
}

/// Each core's load as an htop meter, column-major like Sessions on a `grid` of (cols, rows), then
/// memory across the whole width.
fn draw_system(
    buf: &mut Buffer,
    area: Rect,
    m: &Machine,
    (cols, rows): (usize, usize),
    spin: usize,
) {
    let body = draw_panel(buf, panel(ICON_SYSTEM, "System", vec![]), area);
    let label_w = m.cpus.len().to_string().len();
    if !m.cpus.is_empty() {
        let cores = Rect {
            height: rows as u16,
            ..body
        };
        draw_columns(buf, cores, &m.cpus, cols, rows, |n, &pct, width| {
            vec![cpu_row(n, label_w, pct, width)]
        });
    }
    let mem = Rect {
        y: body.y + rows as u16,
        height: 1,
        ..body
    };
    mem_row(label_w, m.memory, body.width as usize, spin).render(mem, buf);
}

/// `3: [|||||||        23.4%]`. A busy core is normal, so unlike the other meters it never warns.
fn cpu_row(n: usize, label_w: usize, pct: f64, width: usize) -> Line<'static> {
    let mut spans = Vec::from(row_label(n, label_w));
    let text = format!("{pct:.1}%");
    let parts = [(pct / 100.0, GREEN)];
    spans.extend(bar(
        width.saturating_sub(label_w + 2),
        &parts,
        text,
        Style::new().fg(MUTED),
    ));
    Line::from(spans)
}

/// `M: ━━━━━━━━━━╸━━━━━━━  77.5%    12.4G of 16.0G`, drawn like a rate limit (`heat_row`), so its
/// bar ends where theirs do. `M` is right-aligned in `label_w` cells, like the core numbers above.
fn mem_row(label_w: usize, (used, total): (u64, u64), width: usize, spin: usize) -> Line<'static> {
    let pct = used as f64 * 100.0 / total.max(1) as f64;
    let size = vec![
        bytes(used as f64).fg(MUTED),
        " of ".fg(FAINT),
        bytes(total as f64).fg(MUTED),
    ];
    let label = Vec::from(row_label("M", label_w));
    heat_row(label, pct, format!(" {pct:>5.1}%"), size, width, spin)
}

/// Bytes sent (↑) and received (↓) per second, as strips like Tokens, one sample per refresh.
/// Each has its own scale: a download would flatten the uploads next to it.
fn draw_network(buf: &mut Buffer, area: Rect, m: &Machine) {
    let panel = panel(ICON_NETWORK, "Network", vec![" · per second".fg(FAINT)]);
    let cells = (panel.inner(area).width as usize).saturating_sub(1 + 2 + VALUE_W);
    let strips = [("↑", &m.up), ("↓", &m.down)].map(|(label, history)| {
        let visible = &history[history.len().saturating_sub(cells)..];
        let peak = visible.iter().copied().max().unwrap_or(0);
        (label, visible, nice_top(peak as f64, 1_000.0))
    });
    let note = vec![
        format!(
            "1 column = 1s · top ↑ {} ↓ {}",
            bytes(strips[0].2),
            bytes(strips[1].2)
        )
        .fg(FAINT)
        .italic(),
    ];
    let body = draw_panel(buf, with_note(panel, note), area);
    let lines: Vec<Line> = strips
        .iter()
        .flat_map(|&(label, visible, scale)| {
            let last = visible.last().copied().unwrap_or(0);
            let value = count(last, bytes);
            token_strip(
                &row_label(label, 0),
                visible,
                scale,
                cells,
                STRIP_ROWS,
                value,
                0,
            )
        })
        .collect();
    Paragraph::new(lines).render(body, buf);
}

/// Bytes the way htop shows them: 512B, 12.3K, 4.1M, 18.2G, in powers of 1024. It moves up a unit
/// from 1000, so a value never takes more than 6 cells.
fn bytes(n: f64) -> String {
    let mut n = n;
    for unit in ["B", "K", "M", "G"] {
        if n < 1000.0 {
            return if unit == "B" {
                format!("{n:.0}B")
            } else {
                format!("{n:.1}{unit}")
            };
        }
        n /= 1024.0;
    }
    format!("{n:.1}T")
}

fn draw_quota(buf: &mut Buffer, area: Rect, quota: Option<&Quota>, now: u64, spin: usize) {
    let panel = panel(ICON_LIMITS, "Rate limits", vec![]);
    let Some(q) = quota else {
        let body = draw_panel(buf, panel, area);
        empty(
            buf,
            body,
            "no data yet (Pro/Max plans only, needs cc-top --setup)",
        );
        return;
    };
    // Usage outside these sessions (claude.ai, other machines) only shows after the next payload.
    let age = q.updated.elapsed().unwrap_or_default().as_secs();
    let updated = vec![format!("updated {} ago", human(age)).fg(FAINT).italic()];
    let body = draw_panel(buf, with_note(panel, updated), area);
    let lines: Vec<Line> = q
        .limits
        .iter()
        .map(|l| quota_row(l, now / 1000, body.width as usize, spin))
        .collect();
    Paragraph::new(lines).render(body, buf);
}

/// (columns, rows) for `n` meters: one more column every ROWS_PER_COL sessions, while they fit.
fn grid(n: usize, width: u16) -> (usize, usize) {
    let max_cols = (width / MIN_METER_W).max(1) as usize;
    let cols = n.div_ceil(ROWS_PER_COL).clamp(1, max_cols);
    (cols, n.div_ceil(cols))
}

/// `1: [⠋ 09:04][||||||||||||        29.5%]`, the clock is how long the session has been busy.
/// `now` is unix ms.
fn session_row(
    n: usize,
    label_w: usize,
    s: &Session,
    width: usize,
    spin: usize,
    now: u64,
) -> Line<'static> {
    let (icon, clock) = match s.busy_since {
        Some(t) => (
            spinner(spin),
            clock(now.saturating_sub(t) / 1000).fg(TEXT).bold(),
        ),
        None => ("·".fg(FAINT), "00:00".fg(FAINT)),
    };
    let window = s.window.max(1) as f64;
    let used = s.cache_read + s.cache_write + s.uncached;
    let pct = used as f64 * 100.0 / window;
    let text = if s.window == 0 {
        " --".to_string() // no payload yet
    } else {
        format!(" {pct:.1}%")
    };
    let parts = [
        (s.cache_read as f64 / window, GREEN),
        (s.cache_write as f64 / window, YELLOW),
        (s.uncached as f64 / window, RED),
    ];
    let mut spans = Vec::from(row_label(n, label_w));
    spans.extend([
        "[".fg(ORANGE).bold(),
        icon,
        " ".into(),
        clock,
        "]".fg(ORANGE).bold(),
    ]);
    spans.extend(bar(
        width.saturating_sub(label_w + 11),
        &parts,
        text,
        level(pct, Style::new().fg(MUTED), spin),
    ));
    Line::from(spans)
}

/// `   Cc-top Rust project · Opus 5.5 xhigh · up 4h 19m since 13:29 · 41.2M tok · +949 -254`, under the
/// meter. The name is cut to fit; the parts after it are left off from the right when they don't.
fn session_info(label_w: usize, s: &Session, width: usize, now: u64) -> Line<'static> {
    let indent = label_w + 2;
    let mut parts = vec![vec![
        cut(&s.name, width.saturating_sub(indent)).fg(TEXT).bold(),
    ]];
    let mut model = vec![];
    if s.opencode {
        model.push("opencode ".fg(TEAL));
    }
    if !s.model.is_empty() {
        model.push(s.model.clone().fg(MUTED));
    }
    if !model.is_empty() {
        parts.push(model);
    }
    let mut up = vec![
        "up ".fg(FAINT),
        human(now.saturating_sub(s.started)).fg(MUTED),
    ];
    if !s.start.is_empty() {
        up.extend([" since ".fg(FAINT), s.start.clone().fg(MUTED)]);
    }
    parts.push(up);
    parts.push(vec![si(s.total as f64).fg(MUTED), " tok".fg(FAINT)]);
    if let Some((added, removed)) = s.lines {
        parts.push(vec![
            format!("+{added}").fg(GREEN),
            " ".into(),
            format!("-{removed}").fg(RED),
        ]);
    }
    let mut spans = vec![Span::raw(" ".repeat(indent))];
    let mut left = width.saturating_sub(indent);
    for part in parts {
        let sep = if spans.len() > 1 { 3 } else { 0 };
        let w = sep + part.iter().map(Span::width).sum::<usize>();
        if w > left {
            break;
        }
        left -= w;
        if sep > 0 {
            spans.push(" · ".fg(FAINT));
        }
        spans.extend(part);
    }
    Line::from(spans)
}

/// Busy time as `MM:SS`, or ` HhMM` past an hour, so it always takes 5 cells.
fn clock(secs: u64) -> String {
    if secs < 3_600 {
        format!("{:02}:{:02}", secs / 60, secs % 60)
    } else {
        format!("{:>2}h{:02}", secs / 3_600, secs / 60 % 60)
    }
}

/// `5h ━━━━━━━━━━╸━━━━━━━━━━━━  44.0%  resets in 2h 13m`
fn quota_row(l: &Limit, now: u64, width: usize, spin: usize) -> Line<'static> {
    let left = l.resets_at.saturating_sub(now);
    // Past the reset time the saved percentage belongs to the previous window.
    let (pct, text) = if left > 0 {
        (l.used, format!(" {:>5.1}%", l.used))
    } else {
        (0.0, format!(" {:>6}", "--"))
    };
    let resets = if left > 0 { human(left) } else { "now".into() };
    let tail = vec!["  resets in ".fg(FAINT), format!("{resets:>6}").fg(MUTED)];
    let label = vec![format!("{} ", l.name).fg(CYAN).bold()];
    heat_row(label, pct, text, tail, width, spin)
}

/// Cells after a heat row's percentage: `  resets in 3h 44m`.
const TAIL_W: usize = 18;

/// `label ━━━━━━━━━━╸━━━━━━━  44.0%  tail`: a heat bar over what `label`, `text` and `tail` leave
/// of `width`, `text` styled by `level`. The tail is right-aligned in `TAIL_W` cells, so the bars of
/// rows that share a width all end in the same column.
fn heat_row(
    mut spans: Vec<Span<'static>>,
    pct: f64,
    text: String,
    tail: Vec<Span<'static>>,
    width: usize,
    spin: usize,
) -> Line<'static> {
    let width_of = |spans: &[Span]| spans.iter().map(Span::width).sum::<usize>();
    let tail_w = width_of(&tail);
    let line_w = width.saturating_sub(width_of(&spans) + text.len() + tail_w.max(TAIL_W));
    spans.extend(heat_bar(pct / 100.0, line_w));
    spans.push(Span::styled(text, level(pct, Style::new().fg(TEXT), spin)));
    spans.push(" ".repeat(TAIL_W.saturating_sub(tail_w)).into());
    spans.extend(tail);
    Line::from(spans)
}

/// `━━━━━━━━━━╸━━━━━━━━`: `frac` of `width` cells filled to half-cell precision, each filled cell
/// coloured by how far along the bar it is, over a dark track.
fn heat_bar(frac: f64, width: usize) -> Vec<Span<'static>> {
    let halves = ((frac * width as f64 * 2.0).round() as usize).min(width * 2);
    let (full, half) = (halves / 2, halves % 2);
    let at = |i: usize| heat((i as f64 + 0.5) / width as f64);
    let mut spans: Vec<Span> = (0..full).map(|i| "━".fg(at(i))).collect();
    spans.extend((half == 1).then(|| "╸".fg(at(full))));
    // When the fill ends on a cell boundary, a half-cell gap keeps it apart from the track.
    let rest = width - full - half;
    let gap = usize::from(rest > 0 && half == 0 && full > 0);
    spans.push("╺".repeat(gap).fg(TRACK));
    spans.push("━".repeat(rest - gap).fg(TRACK));
    spans
}

/// htop-style `[|||||     29.5%]` filling `width` cells. `parts` are (fraction of the bar, colour),
/// stacked left to right over a dark track; the text sits on top of the bar's tail like in htop.
fn bar(
    width: usize,
    parts: &[(f64, Color)],
    text: String,
    text_style: Style,
) -> Vec<Span<'static>> {
    let inner = width.saturating_sub(2);
    let mut acc = 0.0;
    let ends: Vec<usize> = parts
        .iter()
        .map(|(f, _)| {
            acc += f;
            ((acc * inner as f64).round() as usize).min(inner)
        })
        .collect();
    let mut spans = vec!["[".fg(ORANGE).bold()];
    for j in 0..inner.saturating_sub(text.len()) {
        let color = ends
            .iter()
            .position(|&end| j < end)
            .map_or(TRACK, |k| parts[k].1);
        spans.push("|".fg(color));
    }
    spans.push(Span::styled(text, text_style));
    spans.push("]".fg(ORANGE).bold());
    spans
}

/// Colour at position `t` (0..=1) of a usage bar: teal, green by half, yellow at 80%, red when full.
fn heat(t: f64) -> Color {
    let stops = [(0.0, TEAL), (0.5, GREEN), (0.8, YELLOW), (1.0, RED)];
    let w = stops
        .windows(2)
        .find(|w| t <= w[1].0)
        .unwrap_or(&stops[2..]);
    let ((a, from), (b, to)) = (w[0], w[1]);
    mix(from, to, (t - a) / (b - a))
}

/// The colour `k` (0..=1) of the way from `a` to `b`.
fn mix(a: Color, b: Color, k: f64) -> Color {
    let (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) = (a, b) else {
        return b; // the palette is all RGB
    };
    let k = k.clamp(0.0, 1.0);
    let m = |x: u8, y: u8| (f64::from(x) + (f64::from(y) - f64::from(x)) * k).round() as u8;
    Color::Rgb(m(r1, r2), m(g1, g2), m(b1, b2))
}

/// Warning style shared by every meter: yellow from 80%, red from 90% and pulsing twice a second.
/// We pulse it ourselves because many terminals ignore the blink attribute.
fn level(pct: f64, normal: Style, spin: usize) -> Style {
    match pct {
        90.0.. if (spin / 5).is_multiple_of(2) => Style::new().fg(RED).bold().not_dim(),
        90.0.. => Style::new().fg(RED).dim(),
        80.0.. => Style::new().fg(YELLOW).bold().not_dim(),
        _ => normal,
    }
}

fn human(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, secs / 3_600 % 24, secs / 60 % 60);
    match secs {
        0..60 => format!("{secs}s"),
        60..3_600 => format!("{m}m"),
        3_600..86_400 => format!("{h}h {m:02}m"),
        _ => format!("{d}d {h}h"),
    }
}

#[test]
fn session_info_drops_what_does_not_fit() {
    let s = Session {
        name: "Cc-top Rust project".into(),
        model: "Opus 5.5 xhigh".into(),
        started: 1_000,
        start: "13:29".into(),
        lines: Some((949, 254)),
        total: 41_200_000,
        ..Default::default()
    };
    let text = |width| session_info(1, &s, width, 1_000 + 4 * 3_600 + 19 * 60).to_string();
    assert_eq!(
        text(100),
        "   Cc-top Rust project · Opus 5.5 xhigh · up 4h 19m since 13:29 · 41.2M tok · +949 -254"
    );
    assert_eq!(text(40), "   Cc-top Rust project · Opus 5.5 xhigh");
    assert_eq!(text(12), "   Cc-top R…");
}

#[test]
fn session_row_segments() {
    let now = 1_793_885_392_864;
    let mut s = Session {
        cache_read: 30,
        cache_write: 10,
        uncached: 10,
        window: 100,
        ..Default::default()
    };
    let text = |s: &Session| session_row(1, 1, s, 54, 0, now).to_string();
    assert!(text(&s).starts_with("1: [· 00:00]["));
    assert!(text(&s).ends_with("50.0%]"));
    assert_eq!(text(&s).chars().count(), 54);

    let line = session_row(1, 1, &s, 54, 0, now); // 40-cell bar
    let bars = |c| {
        line.spans
            .iter()
            .filter(|s| s.content == "|" && s.style.fg == Some(c))
            .count()
    };
    assert_eq!([bars(GREEN), bars(YELLOW), bars(RED)], [12, 4, 4]);

    s.busy_since = Some(now - (9 * 60 + 4) * 1000);
    assert!(text(&s).starts_with("1: [⠋ 09:04]["));
    assert_eq!(clock(3_900), " 1h05");
}

#[test]
fn quota_row_counts_down_and_drops_stale_windows() {
    let l = Limit {
        name: "5h",
        used: 44.0,
        resets_at: 1_000 + 2 * 3_600 + 13 * 60,
    };
    let text = |now| quota_row(&l, now, 60, 0).to_string();
    assert!(text(1_000).ends_with("━  44.0%  resets in 2h 13m"));
    assert!(text(99_999).ends_with("━     --  resets in    now"));
    assert_eq!(text(1_000).chars().count(), 60);
    let filled = quota_row(&l, 1_000, 60, 0)
        .spans
        .iter()
        .filter(|s| s.content == "━" && s.style.fg != Some(TRACK))
        .count();
    assert_eq!(filled, 14); // 44% of a 32-cell line
    assert_eq!([heat(0.0), heat(0.8), heat(1.0)], [TEAL, YELLOW, RED]);
}

#[test]
fn token_strip_draws_a_bar_per_bucket() {
    let label = ["1".fg(CYAN).bold(), ": ".fg(FAINT)];
    // 60K, 120K and 15K against a 120K scale: 4, 8 and 1 of 8 levels; padding: baseline.
    let one = token_strip(
        &label,
        &[60_000, 120_000, 15_000],
        120_000.0,
        4,
        1,
        count(15_000, si),
        0,
    );
    assert_eq!(one[0].to_string(), "1: ▁▄█▁  15.0K");
    assert_eq!(one[0].spans[2].style.fg, Some(TRACK));
    // Two rows: 120K fills both, 60K only the bottom one. Higher cells are warmer.
    let two = token_strip(&label, &[60_000, 120_000], 120_000.0, 2, 2, count(0, si), 0);
    assert_eq!(
        [two[0].to_string(), two[1].to_string()],
        ["1:  █      0", "   ██"]
    );
    assert_eq!(two[0].spans[3].style.fg, Some(heat(1.0)));
    assert_eq!(two[1].spans[2].style.fg, Some(heat(0.5)));
    assert_eq!(two[0].spans[4].style.fg, Some(FAINT)); // nothing to count
    // The last 3 columns wait on a response: their empty baseline lights up, a bar stays a bar.
    let wait = token_strip(&label, &[0, 0, 500, 0, 0], 1_000.0, 5, 1, count(0, si), 3);
    let fg: Vec<_> = wait[0].spans[2..7].iter().map(|s| s.style.fg).collect();
    assert_eq!(wait[0].to_string(), "1: ▁▁▄▁▁      0");
    assert_eq!(fg[..2], [Some(TRACK), Some(TRACK)]);
    assert_eq!(fg[3..], [Some(MUTED), Some(MUTED)]);
    assert_ne!(fg[2], Some(MUTED));
    assert_eq!(
        [si(950.0), si(12_345.0), si(4_100_000.0)],
        ["950", "12.3K", "4.1M"]
    );
    assert_eq!(
        [
            nice_top(0.0, 1e3),
            nice_top(10.9e6, 1e3),
            nice_top(1.95e6, 1e3),
            nice_top(0.0, 100.0)
        ],
        [1_200.0, 12e6, 3e6, 120.0]
    );
}

#[test]
fn system_rows_read_like_htop() {
    // 50% of 15 cells inside the brackets: 8 filled, the text over the tail.
    let cpu = cpu_row(3, 1, 50.0, 20);
    assert_eq!(cpu.to_string(), "3: [||||||||||50.0%]");
    let filled = cpu
        .spans
        .iter()
        .filter(|s| s.style.fg == Some(GREEN))
        .count();
    assert_eq!(filled, 8);
    // Lined up with 10+ cores, and with a rate limit at the same width: the bar ends at the same column.
    let mem = mem_row(2, (18 << 30, 36 << 30), 40, 0);
    assert_eq!(mem.to_string(), " M: ━━━━━╸━━━━━  50.0%    18.0G of 36.0G"); // 5½ of 11 cells
    let limit = Limit {
        name: "5h",
        used: 50.0,
        resets_at: 1_000 + 3 * 3_600,
    };
    let bar_end = |l: &Line| l.to_string().chars().position(|c| c == '%');
    assert_eq!(bar_end(&mem), bar_end(&quota_row(&limit, 1_000, 40, 0)));
    assert_eq!(
        [
            bytes(512.0),
            bytes(1_000.0),
            bytes(12.3 * 1024.0),
            bytes(1023.0 * 1024.0)
        ],
        ["512B", "1.0K", "12.3K", "1.0M"]
    );
}

#[test]
fn grid_adds_columns() {
    assert_eq!(grid(0, 100), (1, 0));
    assert_eq!(grid(4, 100), (1, 4));
    assert_eq!(grid(6, 100), (2, 3));
    assert_eq!(grid(30, 100), (3, 10)); // only 3 columns fit, rows grow instead
}

#[test]
fn repo_boxes_wrap_and_cut_long_names() {
    let entries = [
        ("src".into(), true),
        ("README.md".into(), false),
        ("a-very-long-file-name.txt".into(), false),
    ];
    // src/ is 8 wide, README.md 13: both fit in 30 with a gap; the long name wraps.
    let boxes = repo_boxes(&entries, 30);
    let spots: Vec<_> = boxes.iter().map(|(l, r)| (l.as_str(), r.x, r.y)).collect();
    assert_eq!(
        spots,
        [
            ("src/", 0, 0),
            ("README.md", 9, 0),
            ("a-very-long-fil…", 0, 3)
        ]
    );
    assert_eq!(mix(BG, GREEN, 1.0), GREEN);
    assert_eq!(mix(BG, GREEN, 0.0), BG);
}

#[test]
fn entries_skip_dotfiles_and_ignored() {
    let dir = std::env::temp_dir().join(format!("cc-top-entries-{}", std::process::id()));
    for d in ["src", "target", ".git"] {
        fs::create_dir_all(dir.join(d)).unwrap();
    }
    for f in ["README.md", "a.txt", ".gitignore"] {
        fs::write(dir.join(f), "/target\n").unwrap();
    }
    let want = [("src", true), ("a.txt", false), ("README.md", false)];
    assert_eq!(entries(&dir), want.map(|(n, d)| (n.to_string(), d)));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn activity_row_fits_its_width() {
    let text = |t: &Tool, width| activity_row(1, (1, 4), t, width, 112, 0).to_string();
    let mut t = Tool {
        id: "t".into(),
        at: 100,
        name: "Bash".into(),
        arg: "cargo test".into(),
        done: None,
        sub: false,
    };
    assert_eq!(text(&t, 32), "1: ⠋ Bash cargo test       00:12");
    assert_eq!(text(&t, 22), "1: ⠋ Bash cargo… 00:12");
    t.done = Some((103, true));
    assert_eq!(text(&t, 26), "1: ✗ Bash cargo test 00:03");
}
