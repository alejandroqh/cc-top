//! Claude Code's local state: live sessions, plus the status-line payloads our hook saves.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use serde_json::Value;

/// One live session, with the context of its last turn split the way the API reports usage.
#[derive(Default)]
pub struct Session {
    pub id: String,
    pub cache_read: u64, // reused prefix, cheap: healthy sessions are mostly this
    pub cache_write: u64, // newly cached this turn: a thin tip is normal, a long one is a cache miss
    pub uncached: u64,    // input with no caching at all: should be ~0
    pub window: u64,      // 0 until the session's first status-line payload arrives
    pub busy_since: Option<u64>, // unix ms it started working, None while idle
    pub transcript: Option<PathBuf>, // Claude Code's: opencode sessions have none
    pub cwd: PathBuf,
    pub changes: Vec<Change>, // the last CHANGES files edited, oldest first
    pub calls: Vec<Call>, // API calls of the last KEEP_MS, subagents included (`Transcripts::fill`)
    pub pending: Option<u64>, // unix ms the request in flight went out, while one is
    pub tools: Vec<Tool>, // the last ACTIVITY tool calls, oldest first
    pub name: String,
    pub model: String, // `Opus 5.5 xhigh`: display name and effort level, empty before the first payload
    pub started: u64,  // unix secs
    pub start: String, // local `HH:MM` it started, empty on Claude Code versions without `procStart`
    pub lines: Option<(u64, u64)>, // lines added and removed, by every tool (Bash too)
    pub total: u64,    // tokens since the session started, subagents included (`Transcripts::fill`)
    pub opencode: bool, // an opencode session (`opencode::sessions`), not a Claude Code one
}

/// Tool calls kept per session, and shown in the Activity panel.
pub const ACTIVITY: usize = 5;

/// One API call: when its request went out and when its response was written (unix ms), what it
/// read (input, cache write, cache read) and what it wrote (output, thinking included).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Call {
    pub sent: u64,
    pub done: u64,
    pub input: u64,
    pub output: u64,
}

/// Longest a call can take: a request that went out longer ago than this is not the one that
/// answered (a resumed session, an interrupted turn).
const MAX_CALL_MS: u64 = 10 * 60_000;

/// How far back sessions keep their calls: 600 columns of a minute, the widest zoom.
const KEEP_MS: u64 = 600 * 60_000;

impl Call {
    /// Its output written before `t` (unix ms), as if written at an even pace from `sent` to `done`.
    fn written_before(&self, t: u64) -> u64 {
        if t > self.done {
            self.output
        } else if t <= self.sent {
            0
        } else {
            (u128::from(self.output) * u128::from(t - self.sent)
                / u128::from(self.done - self.sent)) as u64
        }
    }
}

/// Tokens read and written per `secs`-long slot, for the `slots` slots up to and including the one
/// holding `now` (unix ms), oldest first. What a call read counts when its request went out; what it
/// wrote is spread over the time it took, so a long answer shows as a band, not a spike.
pub fn buckets(calls: &[Call], now: u64, secs: u64, slots: usize) -> (Vec<u64>, Vec<u64>) {
    let (mut ins, mut outs) = (vec![0; slots], vec![0; slots]);
    if slots == 0 {
        return (ins, outs);
    }
    let span = secs.max(1) * 1000;
    let first = (now / span + 1).saturating_sub(slots as u64);
    let last = first + slots as u64 - 1;
    for c in calls {
        if let Some(slot) = ins.get_mut((c.sent / span).wrapping_sub(first) as usize) {
            *slot += c.input; // older than `first` wraps to a huge index and misses
        }
        for k in (c.sent / span).max(first)..=(c.done / span).min(last) {
            outs[(k - first) as usize] +=
                c.written_before((k + 1) * span) - c.written_before(k * span);
        }
    }
    (ins, outs)
}

/// Tokens read and written from `from` to `to` (unix ms), counted like `buckets`.
pub fn between(calls: &[Call], from: u64, to: u64) -> (u64, u64) {
    calls.iter().fold((0, 0), |(i, o), c| {
        let read = if (from..to).contains(&c.sent) {
            c.input
        } else {
            0
        };
        (i + read, o + c.written_before(to) - c.written_before(from))
    })
}

/// One tool call: it starts in an assistant line and ends with a `tool_result` in a user line.
#[derive(Clone)]
pub struct Tool {
    pub id: String,
    pub at: u64,                   // unix secs it started
    pub name: String,              // `Bash`, or the tool part of an MCP name
    pub arg: String,               // what it works on, on one line
    pub done: Option<(u64, bool)>, // when it finished and whether it failed; None while running
    pub sub: bool,                 // made by one of the session's subagents
}

/// File changes kept per session.
const CHANGES: usize = 16;

/// One Edit or Write that went through, with the line counts its result records, or a Read.
#[derive(Clone)]
pub struct Change {
    pub at: u64, // unix secs
    pub path: PathBuf,
    pub added: usize,
    pub removed: usize,
    pub read: bool, // a Read: no lines either way
}

/// One account-wide rate-limit window.
pub struct Limit {
    pub name: &'static str, // "5h" or "7d"
    pub used: f64,          // percent
    pub resets_at: u64,     // unix seconds
}

pub struct Quota {
    pub limits: Vec<Limit>,
    pub updated: SystemTime, // when the payload it came from was written
}

pub fn config_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::home_dir().unwrap_or_default().join(".claude"))
}

/// Live sessions, oldest first so numbers stay put while new ones start, with what their
/// transcripts say (`Transcripts::fill`). `now` is unix ms.
pub fn sessions(dir: &Path, transcripts: &mut Transcripts, now: u64) -> Vec<Session> {
    let mut found = live_json(&dir.join("sessions"), "claude");
    found.sort_by_key(|s| s["startedAt"].as_u64());
    let mut sessions: Vec<Session> = found
        .iter()
        .map(|s| {
            let id = s["sessionId"].as_str().unwrap_or_default();
            let payload =
                read_json(&dir.join("cc-top").join(format!("{id}.json"))).unwrap_or_default();
            let ctx = &payload["context_window"];
            let usage = &ctx["current_usage"];
            let n = |v: &Value| v.as_u64().unwrap_or(0);
            let cwd = s["cwd"].as_str().unwrap_or_default();
            Session {
                id: id.to_string(),
                cache_read: n(&usage["cache_read_input_tokens"]),
                cache_write: n(&usage["cache_creation_input_tokens"]),
                uncached: n(&usage["input_tokens"]),
                window: n(&ctx["context_window_size"]),
                // statusUpdatedAt is when the session flipped to its current status.
                busy_since: (s["status"] == "busy").then(|| n(&s["statusUpdatedAt"])),
                transcript: Some(
                    payload["transcript_path"]
                        .as_str()
                        .map_or_else(|| transcript_path(dir, cwd, id), PathBuf::from),
                ),
                cwd: PathBuf::from(cwd),
                name: payload["session_name"]
                    .as_str()
                    .or(s["name"].as_str())
                    .unwrap_or_default()
                    .to_string(),
                model: format!(
                    "{} {}",
                    payload["model"]["display_name"]
                        .as_str()
                        .unwrap_or_default(),
                    payload["effort"]["level"].as_str().unwrap_or_default()
                )
                .trim()
                .to_string(),
                started: n(&s["startedAt"]) / 1000,
                // `Thu Nov  5 13:29:27 2026`, from ps: already in local time.
                start: s["procStart"]
                    .as_str()
                    .and_then(|t| t.split_whitespace().nth(3))
                    .map(|t| t.chars().take(5).collect())
                    .unwrap_or_default(),
                lines: payload["cost"]["total_lines_added"]
                    .as_u64()
                    .map(|a| (a, n(&payload["cost"]["total_lines_removed"]))),
                ..Default::default()
            }
        })
        .collect();
    transcripts.fill(&mut sessions, now);
    sessions
}

/// Where Claude Code keeps a session's transcript: the project dir is the cwd with every
/// non-alphanumeric character turned into `-`.
fn transcript_path(dir: &Path, cwd: &str, id: &str) -> PathBuf {
    let project: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    dir.join("projects")
        .join(project)
        .join(format!("{id}.jsonl"))
}

/// Token use per API call and the latest tool calls, read from the transcripts. They grow to tens
/// of MB, so each one is read once and then only its new lines.
#[derive(Default)]
pub struct Transcripts(HashMap<PathBuf, Transcript>);

#[derive(Default)]
struct Transcript {
    offset: u64,
    // Message ids, to their call: every content block repeats its message's usage, and is
    // written as it completes, so the last one says when the response was done.
    seen: HashMap<String, usize>,
    calls: Vec<Call>,
    // The last user or assistant line: whether it was a user one (a prompt or tool results,
    // after which a request goes out), and when (unix ms).
    last: Option<(bool, u64)>,
    tools: Vec<Tool>,
    changes: Vec<Change>,
}

impl Transcripts {
    /// Sets each session's `calls`, `pending`, `tools`, `changes` and `total`, the session's
    /// subagents included. `now` is unix ms. Forgets ended sessions.
    fn fill(&mut self, sessions: &mut [Session], now: u64) {
        let mut used = HashSet::new();
        for s in sessions {
            let Some(main) = s.transcript.clone() else {
                continue;
            };
            s.calls.clear();
            s.pending = None;
            s.tools.clear();
            s.changes.clear();
            s.total = 0;
            // Subagents write transcripts of their own, in a dir named after the session's.
            let subagents = files(&main.with_extension("").join("subagents"), "jsonl");
            for (i, path) in std::iter::once(main).chain(subagents).enumerate() {
                let t = self.0.entry(path.clone()).or_default();
                let _ = t.read_new(&path); // missing until the first message: try again next time
                let recent = t.calls.iter().filter(|c| c.done + KEEP_MS >= now);
                s.calls.extend(recent);
                s.total += t.calls.iter().map(|c| c.input + c.output).sum::<u64>();
                // A transcript waiting on a response, in a session that is working. A subagent
                // that finished ends on its answer, so only running ones count.
                if s.busy_since.is_some()
                    && let Some((true, sent)) = t.last
                    && sent + MAX_CALL_MS >= now
                {
                    s.pending = Some(s.pending.map_or(sent, |p| p.min(sent)));
                }
                let sub = i > 0;
                s.tools
                    .extend(t.tools.iter().map(|t| Tool { sub, ..t.clone() }));
                s.changes.extend_from_slice(&t.changes);
                used.insert(path);
            }
            s.tools.sort_by_key(|t| t.at); // stable: the same second keeps file order
            keep_last(&mut s.tools, ACTIVITY);
            s.changes.sort_by_key(|c| c.at);
            keep_last(&mut s.changes, CHANGES);
        }
        self.0.retain(|path, _| used.contains(path));
    }
}

/// Drops all but the last `n` items of `v`.
fn keep_last<T>(v: &mut Vec<T>, n: usize) {
    v.drain(..v.len().saturating_sub(n));
}

impl Transcript {
    fn read_new(&mut self, path: &Path) -> io::Result<()> {
        let len = fs::metadata(path)?.len();
        if len == self.offset {
            return Ok(()); // nothing new: most transcripts, most refreshes
        }
        if len < self.offset {
            *self = Self::default(); // rewritten: start over
        }
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut buf = Vec::new();
        file.take(len - self.offset).read_to_end(&mut buf)?;
        // Only whole lines: Claude Code may be halfway through writing the last one.
        let end = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        self.offset += end as u64;
        for line in buf[..end].split(|&b| b == b'\n') {
            // Parsing is the slow part, and most lines carry neither tokens nor tool calls.
            let has = |s: &[u8]| line.windows(s.len()).any(|w| w == s);
            // User lines say when the next request goes out, even a prompt with no tool result.
            let wanted: [&[u8]; 5] = [
                b"\"usage\":",
                b"compact_boundary",
                b"\"tool_use\"",
                b"\"tool_result\"",
                b"\"type\":\"user\"",
            ];
            if !wanted.iter().any(|s| has(s)) {
                continue;
            }
            let Ok(v) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            let Some(ms) = v["timestamp"].as_str().and_then(timestamp_ms) else {
                continue;
            };
            self.log_tools(&v, ms / 1000);
            let m = &v["message"];
            let usage = |k: &str| m["usage"][k].as_u64().unwrap_or(0);
            if v["subtype"] == "compact_boundary" {
                // The summarising call logs no usage. It read the whole conversation, which
                // the boundary records; its output (the summary) is not recorded anywhere.
                let read = v["compactMetadata"]["preTokens"].as_u64().unwrap_or(0);
                self.calls.push(Call {
                    sent: ms,
                    done: ms,
                    input: read,
                    output: 0,
                });
            } else if v["type"] == "assistant"
                && let Some(id) = m["id"].as_str()
            {
                match self.seen.get(id) {
                    Some(&i) => self.calls[i].done = self.calls[i].done.max(ms),
                    None => {
                        // It went out after the line before it: the prompt or the tool results.
                        let sent = self
                            .last
                            .map_or(ms, |(_, at)| at)
                            .clamp(ms.saturating_sub(MAX_CALL_MS), ms);
                        self.seen.insert(id.to_string(), self.calls.len());
                        self.calls.push(Call {
                            sent,
                            done: ms,
                            input: usage("input_tokens")
                                + usage("cache_creation_input_tokens")
                                + usage("cache_read_input_tokens"),
                            output: usage("output_tokens"),
                        });
                    }
                }
            }
            if v["type"] == "user" || v["type"] == "assistant" {
                self.last = Some((v["type"] == "user", ms));
            }
        }
        Ok(())
    }

    /// Tool calls, and the file changes their results record.
    fn log_tools(&mut self, v: &Value, at: u64) {
        let cwd = v["cwd"].as_str().unwrap_or_default();
        for b in v["message"]["content"].as_array().into_iter().flatten() {
            if b["type"] == "tool_use" && v["type"] == "assistant" {
                let name = b["name"].as_str().unwrap_or_default();
                self.tools.push(Tool {
                    id: b["id"].as_str().unwrap_or_default().to_string(),
                    at,
                    name: name.rsplit("__").next().unwrap_or(name).to_string(), // mcp__server__tool
                    arg: tool_arg(&b["input"], cwd),
                    done: None,
                    sub: false,
                });
                // From the call: not every Read result records its path (images, repeat reads).
                // ponytail: a Read that then fails still counts.
                if name == "Read"
                    && let Some(path) = b["input"]["file_path"].as_str()
                {
                    self.changes.push(Change {
                        at,
                        path: PathBuf::from(path),
                        added: 0,
                        removed: 0,
                        read: true,
                    });
                }
            } else if b["type"] == "tool_result"
                && v["type"] == "user"
                && let Some(t) = self
                    .tools
                    .iter_mut()
                    .find(|t| b["tool_use_id"] == t.id.as_str())
            {
                t.done = Some((at, b["is_error"] == true));
            }
        }
        // An Edit or Write result: its diff, with `+` / `-` lines, or the whole new file.
        let r = &v["toolUseResult"];
        if v["type"] == "user"
            && let Some(path) = r["filePath"].as_str()
            && let Some(patch) = r["structuredPatch"].as_array()
        {
            let lines: Vec<&str> = patch
                .iter()
                .flat_map(|hunk| hunk["lines"].as_array().into_iter().flatten())
                .filter_map(Value::as_str)
                .collect();
            let count = |c: char| lines.iter().filter(|l| l.starts_with(c)).count();
            let added = match r["type"].as_str() {
                Some("create") => r["content"].as_str().map_or(0, |c| c.lines().count()),
                _ => count('+'),
            };
            self.changes.push(Change {
                at,
                path: PathBuf::from(path),
                added,
                removed: count('-'),
                read: false,
            });
        }
        keep_last(&mut self.tools, ACTIVITY);
        keep_last(&mut self.changes, CHANGES);
    }
}

/// The input that says what a tool call works on, on one line, with paths relative to the session.
/// `filePath` and `path` are opencode's.
pub fn tool_arg(input: &Value, cwd: &str) -> String {
    let keys = [
        "command",
        "file_path",
        "filePath",
        "pattern",
        "path",
        "url",
        "query",
        "description",
        "skill",
        "prompt",
    ];
    let s = keys
        .iter()
        .find_map(|k| input[k].as_str())
        .or_else(|| input.as_object()?.values().find_map(Value::as_str))
        .unwrap_or_default();
    let mut s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if !cwd.is_empty() {
        s = s.replace(&format!("{cwd}/"), "");
    }
    match std::env::home_dir() {
        Some(home) => s.replace(&format!("{}/", home.display()), "~/"),
        None => s,
    }
}

/// `2026-11-05T13:29:52.864Z` to unix milliseconds. Transcripts always write UTC.
fn timestamp_ms(ts: &str) -> Option<u64> {
    let n = |a: usize, b: usize| -> Option<i64> { ts.get(a..b)?.parse().ok() };
    let (y, mo, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    // Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's days_from_civil).
    let y = if mo <= 2 { y - 1 } else { y };
    let (era, yoe) = (y.div_euclid(400), y.rem_euclid(400));
    let doy = (153 * ((mo + 9) % 12) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    let secs = days * 86_400 + n(11, 13)? * 3_600 + n(14, 16)? * 60 + n(17, 19)?;
    u64::try_from(secs * 1000 + n(20, 23).unwrap_or(0)).ok()
}

/// Rate limits are per account, so any session's newest payload has them (Pro/Max plans only).
pub fn quota(dir: &Path) -> Option<Quota> {
    let (updated, path) = files(&dir.join("cc-top"), "json")
        .filter_map(|p| Some((fs::metadata(&p).ok()?.modified().ok()?, p)))
        .max()?;
    let rl = &read_json(&path)?["rate_limits"];
    let limits: Vec<Limit> = [("5h", "five_hour"), ("7d", "seven_day")]
        .into_iter()
        .filter_map(|(name, key)| {
            let used = rl[key]["used_percentage"].as_f64()?;
            Some(Limit {
                name,
                used,
                resets_at: rl[key]["resets_at"].as_u64().unwrap_or(0),
            })
        })
        .collect();
    (!limits.is_empty()).then_some(Quota { limits, updated })
}

/// `cc-top --setup`: point Claude Code's status line at this binary.
pub fn setup() -> Result<(), String> {
    let exe = std::env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|e| e.to_string())?;
    let exe = exe.display().to_string();
    let cmd = if exe.contains(char::is_whitespace) {
        format!("'{exe}' statusline")
    } else {
        format!("{exe} statusline")
    };

    let (real, old) = read_settings()?;
    write_settings(&real, &with_statusline(old.as_deref(), &cmd)?)?;
    println!("status line: {cmd}\nwritten to:  {}", real.display());
    println!("Running Claude Code sessions pick it up within seconds.");
    if exe.contains("/target/") {
        println!("note: this is a cargo build dir; for a path that survives `cargo clean`,");
        println!("      run `cargo install --path .` and then `cc-top --setup`.");
    }
    Ok(())
}

/// `cc-top --uninstall`: undo `--setup` and delete the saved payloads. Leaves the binary.
pub fn uninstall() -> Result<(), String> {
    let (real, old) = read_settings()?;
    match without_statusline(old.as_deref())? {
        Some(new) => {
            write_settings(&real, &new)?;
            println!("removed the cc-top status line from {}", real.display());
        }
        None => println!("no cc-top status line in {}", real.display()),
    }
    remove_dir(&config_dir().join("cc-top"))?;
    println!("`cargo uninstall cc-top` removes the binary.");
    Ok(())
}

/// Deletes `dir` and what it holds, when it is there.
pub fn remove_dir(dir: &Path) -> Result<(), String> {
    match fs::remove_dir_all(dir) {
        Ok(()) => println!("deleted {}", dir.display()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    }
    Ok(())
}

/// Where settings.json really is (through a symlinked dotfile, so we write it rather than
/// replace the link), and its text, None when there is no file yet.
fn read_settings() -> Result<(PathBuf, Option<String>), String> {
    let file = config_dir().join("settings.json");
    let real = fs::canonicalize(&file).unwrap_or(file);
    match fs::read_to_string(&real) {
        Ok(s) => Ok((real, Some(s))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok((real, None)),
        Err(e) => Err(format!("{}: {e}", real.display())),
    }
}

fn write_settings(real: &Path, new: &str) -> Result<(), String> {
    write_atomic(real, &real.with_extension("json.cc-top.tmp"), new)
        .map_err(|e| format!("{}: {e}", real.display()))
}

/// Writes `path` by way of `tmp`, so a reader never sees half a file, and keeps the mode `path`
/// had (settings can hold secrets).
fn write_atomic(path: &Path, tmp: &Path, text: &str) -> io::Result<()> {
    fs::write(tmp, text)?;
    if let Ok(meta) = fs::metadata(path) {
        fs::set_permissions(tmp, meta.permissions())?;
    }
    fs::rename(tmp, path)
}

/// `settings` with our status line set, or why we won't touch it.
fn with_statusline(settings: Option<&str>, cmd: &str) -> Result<String, String> {
    let mut obj = parse_settings(settings)?;
    if let Some(old) = obj.get("statusLine")
        && !is_ours(old)
    {
        let old = old["command"].as_str().unwrap_or_default();
        return Err(format!(
            "statusLine already runs `{old}`; remove it from settings.json first"
        ));
    }
    obj.insert(
        "statusLine".into(),
        serde_json::json!({ "type": "command", "command": cmd }),
    );
    Ok(pretty(&obj))
}

/// `settings` without our status line, or None when it has none of ours to remove.
fn without_statusline(settings: Option<&str>) -> Result<Option<String>, String> {
    let mut obj = parse_settings(settings)?;
    if !obj.get("statusLine").is_some_and(is_ours) {
        return Ok(None);
    }
    obj.remove("statusLine");
    Ok(Some(pretty(&obj)))
}

fn parse_settings(settings: Option<&str>) -> Result<serde_json::Map<String, Value>, String> {
    match settings.map(serde_json::from_str::<Value>) {
        None => Ok(Default::default()),
        Some(Ok(Value::Object(obj))) => Ok(obj),
        Some(Ok(_)) => Err("settings.json is not a JSON object".into()),
        Some(Err(e)) => Err(format!(
            "settings.json is not valid JSON ({e}); fix it first"
        )),
    }
}

fn is_ours(status_line: &Value) -> bool {
    let cmd = status_line["command"].as_str().unwrap_or_default();
    cmd.contains("cc-top") && cmd.ends_with(" statusline")
}

/// Sorted keys + 2-space indent is also how Claude Code writes settings.json, so diffs stay minimal.
fn pretty(obj: &serde_json::Map<String, Value>) -> String {
    serde_json::to_string_pretty(obj).expect("a JSON map always serializes")
}

/// `cc-top statusline`: run by Claude Code as its status-line command after each update.
/// Saves the payload per session for the TUI and prints nothing, so the status line stays blank.
pub fn save_statusline() -> io::Result<()> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let payload: Value = serde_json::from_str(&input)?;
    let id = payload["session_id"].as_str().unwrap_or_default();
    // It becomes a file name, so allow only UUID characters.
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Ok(());
    }
    let out = config_dir().join("cc-top");
    fs::create_dir_all(&out)?;
    let tmp = out.join(format!(".{id}.{}.tmp", std::process::id()));
    write_atomic(&out.join(format!("{id}.json")), &tmp, &input)?;
    prune(&out, Duration::from_secs(7 * 86_400));
    Ok(())
}

/// Deletes payloads (and temp files left by a crash) not written for `age`: their sessions ended.
/// One still open just writes its payload again on its next update. The payload rate limits come
/// from is the newest, so it is never old.
fn prune(dir: &Path, age: Duration) {
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = e.path();
        let ours = path.extension().is_some_and(|x| x == "json" || x == "tmp");
        let modified = e.metadata().and_then(|m| m.modified());
        if ours && modified.is_ok_and(|t| t.elapsed().unwrap_or_default() > age) {
            let _ = fs::remove_file(path); // another hook may have deleted it first
        }
    }
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// The files directly in `dir` named `*.ext`.
fn files(dir: &Path, ext: &'static str) -> impl Iterator<Item = PathBuf> + use<> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(move |p| p.extension().is_some_and(|x| x == ext))
}

/// The JSON files in `dir` whose `pid` is still running a program named `name`: they outlive a
/// process that crashed or was killed.
pub fn live_json(dir: &Path, name: &str) -> Vec<Value> {
    let mut found: Vec<Value> = files(dir, "json").filter_map(|p| read_json(&p)).collect();
    let alive = live_pids(found.iter().filter_map(|f| f["pid"].as_u64()), name);
    found.retain(|f| f["pid"].as_u64().is_some_and(|p| alive.contains(&p)));
    found
}

/// Those of `pids` that are still running a program named `name`.
fn live_pids(pids: impl Iterator<Item = u64>, name: &str) -> HashSet<u64> {
    let list: Vec<String> = pids.map(|p| p.to_string()).collect();
    if list.is_empty() {
        return HashSet::new();
    }
    let Ok(out) = Command::new("ps")
        .args(["-p", &list.join(","), "-o", "pid=,comm="])
        .output()
    else {
        return HashSet::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.trim_end().ends_with(name)) // a reused PID belongs to some other program
        .filter_map(|l| l.split_whitespace().next()?.parse().ok())
        .collect()
}

#[test]
fn setup_keeps_settings_and_refuses_foreign_status_lines() {
    let cmd = "/bin/cc-top statusline";
    let new = with_statusline(None, cmd).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&new).unwrap()["statusLine"]["command"],
        cmd
    );

    let mine = with_statusline(
        Some(r#"{"theme":"dark","statusLine":{"command":"/old/cc-top statusline"}}"#),
        cmd,
    );
    let mine: Value = serde_json::from_str(&mine.unwrap()).unwrap();
    assert_eq!(
        (
            mine["theme"].as_str(),
            mine["statusLine"]["command"].as_str()
        ),
        (Some("dark"), Some(cmd))
    );

    assert!(with_statusline(Some(r#"{"statusLine":{"command":"~/abtop.sh"}}"#), cmd).is_err());
    assert!(with_statusline(Some("{oops"), cmd).is_err());

    // --uninstall takes out only our status line, and refuses nothing.
    let mine = r#"{"theme":"dark","statusLine":{"command":"/x/cc-top statusline"}}"#;
    let left = without_statusline(Some(mine)).unwrap().unwrap();
    assert_eq!(left, "{\n  \"theme\": \"dark\"\n}");
    let foreign = r#"{"statusLine":{"command":"~/abtop.sh"}}"#;
    assert_eq!(without_statusline(Some(foreign)), Ok(None));
    assert_eq!(without_statusline(None), Ok(None));
    assert!(without_statusline(Some("[1]")).is_err());
}

#[test]
fn prune_deletes_only_old_payloads() {
    let dir = std::env::temp_dir().join(format!("cc-top-prune-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let old = SystemTime::now() - Duration::from_secs(8 * 86_400);
    for name in ["old.json", ".old.1.tmp", "old.txt", "new.json"] {
        let f = File::create(dir.join(name)).unwrap();
        if name.starts_with("old") || name.starts_with(".old") {
            f.set_modified(old).unwrap();
        }
    }
    prune(&dir, Duration::from_secs(7 * 86_400));
    let mut left: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(left, ["new.json", "old.txt"]); // not ours: left alone
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn transcript_usage_counts_each_message_once() {
    assert_eq!(timestamp_ms("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(
        timestamp_ms("2024-02-29T00:00:00Z"),
        Some(1_709_164_800_000)
    );
    assert_eq!(
        timestamp_ms("2026-11-05T13:29:52.864Z"),
        Some(1_793_885_392_864)
    );
    assert_eq!(timestamp_ms("1970-01-01T00:00:12Z"), Some(12_000));

    let dir = std::env::temp_dir().join(format!("cc-top-test-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.jsonl");
    let call = |id: &str, ts: &str| {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"id":"{id}","usage":{{"input_tokens":1,"cache_creation_input_tokens":10,"cache_read_input_tokens":100,"output_tokens":1000}}}}}}"#
        )
    };
    let a = call("a", "1970-01-01T00:00:10Z");
    // Message b's two content blocks: the response was done when the second was written.
    let (b1, b2) = (
        call("b", "1970-01-01T00:00:19Z"),
        call("b", "1970-01-01T00:00:20Z"),
    );
    let user = r#"{"type":"user","timestamp":"1970-01-01T00:00:12Z"}"#; // b went out then
    // Two content blocks of message a, a user line, then half of message b.
    fs::write(&path, format!("{a}\n{a}\n{user}\n{}", &b1[..20])).unwrap();
    let mut s = [Session {
        transcript: Some(path.clone()),
        ..Default::default()
    }];
    let mut tokens = Transcripts::default();
    tokens.fill(&mut s, 25_000);
    let a_call = Call {
        sent: 10_000, // nothing before it: as if it took no time
        done: 10_000,
        input: 111,
        output: 1000,
    };
    assert_eq!(s[0].calls, [a_call]);
    assert_eq!(
        buckets(&s[0].calls, 25_000, 10, 3),
        (vec![0, 111, 0], vec![0, 1000, 0])
    );
    assert_eq!(s[0].pending, None); // b is in flight, but the session is idle

    // The rest of b, then a compaction, which logs only the tokens it read.
    let compact = r#"{"type":"system","subtype":"compact_boundary","timestamp":"1970-01-01T00:00:21Z","compactMetadata":{"preTokens":5000}}"#;
    // Then six tool calls, of which only the last ACTIVITY are kept, and the result of one.
    let tool = |id: &str, cmd: &str| {
        format!(
            r#"{{"type":"assistant","timestamp":"1970-01-01T00:00:22Z","cwd":"/p","message":{{"content":[{{"type":"tool_use","id":"{id}","name":"Bash","input":{{"command":"{cmd}"}}}}]}}}}"#
        )
    };
    let tools = [
        ("t1", "ls"),
        ("t2", "b"),
        ("t3", "c"),
        ("t4", r"cat /p/src/main.rs\n| head"),
        ("t5", "d"),
        ("t6", "e"),
    ]
    .map(|(id, cmd)| tool(id, cmd))
    .join("\n");
    let failed = r#"{"type":"user","timestamp":"1970-01-01T00:00:25Z","message":{"content":[{"type":"tool_result","tool_use_id":"t3","is_error":true}]}}"#;
    fs::write(
        &path,
        format!("{a}\n{a}\n{user}\n{b1}\n{b2}\n{compact}\n{tools}\n{failed}\n"),
    )
    .unwrap();
    // A subagent: one call and one tool call, in the same second as t2-t6 but in another file.
    let agents = dir.join("t").join("subagents");
    fs::create_dir_all(&agents).unwrap();
    let agent = format!(
        "{}\n{}\n",
        call("s", "1970-01-01T00:00:22Z"),
        tool("x", "e")
    );
    fs::write(agents.join("agent-x.jsonl"), agent).unwrap();
    s[0].busy_since = Some(0);
    tokens.fill(&mut s, 25_000);
    let (b_call, compact_call, s_call) = (
        Call {
            sent: 12_000,
            done: 20_000,
            ..a_call
        },
        Call {
            sent: 21_000,
            done: 21_000,
            input: 5000,
            output: 0,
        },
        Call {
            sent: 22_000,
            done: 22_000,
            ..a_call
        },
    );
    assert_eq!(s[0].calls, [a_call, b_call, compact_call, s_call]);
    // In 5s slots from 5s: b read at 12s, and wrote from 12s to 20s, 3/8 of it before 15s.
    assert_eq!(
        buckets(&s[0].calls, 25_000, 5, 5),
        (vec![0, 222, 0, 5111, 0], vec![0, 1375, 625, 1000, 0])
    );
    assert_eq!(between(&s[0].calls, 15_000, 25_000), (5111, 1625));
    // t3's result went in at 25s, so a request is out since then; the subagent has its answer.
    assert_eq!(s[0].pending, Some(25_000));
    let t = &s[0].tools;
    assert_eq!(
        t.iter().map(|t| (t.id.as_str(), t.sub)).collect::<Vec<_>>(),
        [
            ("t3", false),
            ("t4", false),
            ("t5", false),
            ("t6", false),
            ("x", true)
        ]
    );
    assert_eq!(t[1].arg, "cat src/main.rs | head");
    assert_eq!(
        [t[0].done, t[1].done, t[4].done],
        [Some((25, true)), None, None]
    );
    tokens.fill(&mut s, 35_000);
    // a and b scrolled out
    assert_eq!(
        buckets(&s[0].calls, 35_000, 10, 2),
        (vec![5111, 0], vec![1000, 0])
    );
    assert_eq!(s[0].total, 3 * 1111 + 5000); // but still count
    tokens.fill(&mut s, 25_000 + MAX_CALL_MS + 1);
    assert_eq!(s[0].pending, None); // out too long to be the request that answers
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn edits_count_lines() {
    let dir = std::env::temp_dir().join(format!("cc-top-edits-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.jsonl");
    let edit = r#"{"type":"user","timestamp":"1970-01-01T00:00:05Z","toolUseResult":{"filePath":"/p/src/a.rs","structuredPatch":[{"lines":[" keep","-old","+new","+more"]}]},"message":{"content":[{"type":"tool_result","tool_use_id":"e"}]}}"#;
    let create = r#"{"type":"user","timestamp":"1970-01-01T00:00:06Z","toolUseResult":{"type":"create","filePath":"/p/b.md","content":"1\n2\n3\n","structuredPatch":[]},"message":{"content":[{"type":"tool_result","tool_use_id":"w"}]}}"#;
    let read = r#"{"type":"assistant","timestamp":"1970-01-01T00:00:07Z","message":{"content":[{"type":"tool_use","id":"r","name":"Read","input":{"file_path":"/p/README.md"}}]}}"#;
    fs::write(&path, format!("{edit}\n{create}\n{read}\n")).unwrap();
    let mut t = Transcript::default();
    t.read_new(&path).unwrap();
    let got: Vec<_> = t
        .changes
        .iter()
        .map(|c| (c.at, c.path.to_str().unwrap(), c.added, c.removed, c.read))
        .collect();
    let want = [
        (5, "/p/src/a.rs", 2, 1, false),
        (6, "/p/b.md", 3, 0, false),
        (7, "/p/README.md", 0, 0, true),
    ];
    assert_eq!(got, want);
    fs::remove_dir_all(dir).unwrap();
}
