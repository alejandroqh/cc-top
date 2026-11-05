//! opencode sessions, from the files our plugin (`opencode.js`) writes in each opencode window.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::claude::{self, Call, Change, Session, Tool};

const PLUGIN: &str = include_str!("opencode.js");

/// Sessions open in live opencode windows, oldest first, with `calls` like `Transcripts::fill`.
/// `dir` is Claude Code's config dir, where the plugin writes too.
pub fn sessions(dir: &Path) -> Vec<Session> {
    let windows = claude::live_json(&dir.join("cc-top").join("opencode"), "opencode");
    let mut seen = HashSet::new(); // two windows can have the same session open
    let mut found: Vec<Session> = windows
        .iter()
        .flat_map(|f| f["sessions"].as_array().into_iter().flatten())
        .filter(|s| seen.insert(s["id"].as_str().unwrap_or_default().to_string()))
        .map(session)
        .collect();
    found.sort_by_key(|s| s.started);
    found
}

fn session(s: &Value) -> Session {
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    let text = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let list = |k: &str| s[k].as_array().into_iter().flatten();
    let cwd = s["cwd"].as_str().unwrap_or_default();
    // `pending` stays None: a message spans its tool runs too, so it can't tell one apart.
    Session {
        id: text(&s["id"]),
        cache_read: n(&s["context"]["read"]),
        cache_write: n(&s["context"]["write"]),
        uncached: n(&s["context"]["input"]),
        window: n(&s["window"]),
        busy_since: s["busy_since"].as_u64(),
        cwd: PathBuf::from(cwd),
        changes: list("changes")
            .map(|c| Change {
                at: n(&c["at"]),
                path: PathBuf::from(text(&c["path"])),
                added: n(&c["added"]) as usize,
                removed: n(&c["removed"]) as usize,
                read: c["read"] == true,
            })
            .collect(),
        calls: list("calls")
            .map(|c| Call {
                sent: n(&c[0]),
                done: n(&c[1]),
                input: n(&c[2]),
                output: n(&c[3]),
            })
            .collect(),
        tools: list("tools")
            .map(|t| Tool {
                id: text(&t["id"]),
                at: n(&t["at"]),
                name: text(&t["name"]),
                arg: claude::tool_arg(&t["input"], cwd),
                done: t["done"].as_array().map(|d| (n(&d[0]), d[1] == true)),
                sub: t["sub"] == true,
            })
            .collect(),
        name: text(&s["name"]),
        model: text(&s["model"]),
        started: n(&s["started"]),
        start: text(&s["start"]),
        lines: Some((n(&s["lines"][0]), n(&s["lines"][1]))),
        total: n(&s["total"]),
        opencode: true,
        ..Default::default()
    }
}

/// Where opencode finds global plugins: `$XDG_CONFIG_HOME/opencode/plugins`, else under ~/.config.
fn plugins_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::home_dir().unwrap_or_default().join(".config"))
        .join("opencode")
        .join("plugins")
}

/// `cc-top --setup`: install the plugin, when opencode is set up here.
pub fn setup() -> Result<(), String> {
    let plugins = plugins_dir();
    let config = plugins.parent().unwrap_or(&plugins);
    if !config.is_dir() {
        println!("opencode: no {}, skipped", config.display());
        return Ok(());
    }
    let file = plugins.join("cc-top").join("tui.js");
    fs::create_dir_all(plugins.join("cc-top"))
        .and_then(|()| fs::write(&file, PLUGIN))
        .map_err(|e| format!("{}: {e}", file.display()))?;
    println!("opencode plugin: {}", file.display());
    println!("Restart opencode windows to load it.");
    Ok(())
}

/// `cc-top --uninstall`: remove the plugin. `claude::uninstall` deletes what it wrote.
pub fn uninstall() -> Result<(), String> {
    claude::remove_dir(&plugins_dir().join("cc-top"))
}

/// The plugin, run by node against a stand-in for opencode's API, writes what `session` reads.
#[test]
fn plugin_writes_what_cc_top_reads() {
    const SCRIPT: &str = r#"
import fs from "node:fs";
import path from "node:path";
const T = 1793900000000;
const tok = (input, output, read, write) => ({ input, output, reasoning: 1, cache: { read, write } });
const tool = (id, name, status, input, created, completed, metadata) =>
  ({ type: "tool", id, name, state: { status, input, metadata }, time: { created, completed } });
const sessions = {
  ses_a: { id: "ses_a", title: "Fix the parser", location: { directory: "/p" },
    model: { id: "glm", providerID: "litellm", variant: "high" }, time: { created: T, busy: T + 50000 }, tokens: tok(10, 20, 300, 40) },
  ses_k: { id: "ses_k", parentID: "ses_a", location: { directory: "/p" }, time: { created: T + 5000 }, tokens: tok(1, 2, 3, 4) },
};
const diff = "Index: /p/src/b.rs\n===\n--- /p/src/b.rs\n+++ /p/src/b.rs\n@@ -1,2 +1,2 @@\n-old\n--x\n+new\n ctx\n" +
  "Index: /p/c.md\n===\n--- /p/c.md\n+++ /p/c.md\n@@ -0,0 +1 @@\n+hi\n";
const messages = {
  ses_a: [
    { type: "user", time: { created: T } },
    { type: "assistant", time: { created: T + 1000, completed: T + 9000 }, tokens: tok(5, 6, 100, 20), content: [
      tool("c1", "read", "completed", { path: "/p/src/a.rs" }, T + 2000, T + 2100),
      tool("c2", "apply_patch", "completed", { patchText: "*** Begin Patch" }, T + 3000, T + 4000, { diff }),
      tool("c3", "write", "error", { path: "/p/d.rs", content: "x\n" }, T + 4500, T + 4600),
    ] },
    { type: "assistant", time: { created: T + 60000 }, tokens: tok(7, 0, 200, 30), content: [
      tool("c4", "shell", "running", { command: "cargo  test   -q", description: "Run tests" }, T + 61000),
    ] },
  ],
  ses_k: [
    { type: "assistant", time: { created: T + 6000, completed: T + 7000 }, tokens: tok(1, 1, 1, 1), content: [
      tool("k1", "write", "completed", { filePath: "/p/e.txt", content: "a\nb" }, T + 6500, T + 6800),
      tool("k2", "grep", "completed", { pattern: "fn main", path: "/p" }, T + 6900, T + 6950),
    ] },
  ],
};
const api = {
  ui: {
    tabs: { enabled: () => true, list: () => [{ sessionID: "ses_a" }] },
    router: { current: () => ({ type: "session", sessionID: "ses_k" }) }, // its subagent, on screen
  },
  data: {
    session: {
      get: (id) => sessions[id],
      root: (id) => sessions[id]?.parentID ?? id,
      family: () => Object.keys(sessions),
      status: (id) => (id === "ses_a" ? "running" : "idle"),
      message: { list: (id) => messages[id] },
    },
    location: { model: { list: () => [{ id: "glm", providerID: "litellm", name: "GLM 5", limit: { context: 200000 } }] } },
  },
};
const { default: plugin } = await import(process.argv[1]);
plugin.setup(api); // writes once right away
const file = path.join(process.env.CLAUDE_CONFIG_DIR, "cc-top", "opencode", `${process.pid}.json`);
process.stdout.write(fs.readFileSync(file));
process.exit(0);
"#;
    let dir = std::env::temp_dir().join(format!("cc-top-opencode-{}", std::process::id()));
    let plugin = concat!(env!("CARGO_MANIFEST_DIR"), "/src/opencode.js");
    let Ok(out) = std::process::Command::new("node")
        .args(["--input-type=module", "-e", SCRIPT, plugin])
        .env("CLAUDE_CONFIG_DIR", &dir)
        .output()
    else {
        eprintln!("no node to run the plugin with: skipped");
        return;
    };
    let _ = fs::remove_dir_all(&dir);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let file: Value = serde_json::from_slice(&out.stdout).unwrap();
    let all = file["sessions"].as_array().unwrap();
    assert_eq!(all.len(), 1); // the subagent on screen is its parent's tab
    let t0 = 1_793_900_000;
    let s = session(&all[0]);
    assert_eq!(
        (s.name.as_str(), s.model.as_str(), s.window),
        ("Fix the parser", "GLM 5 high", 200_000)
    );
    assert_eq!((s.cache_read, s.cache_write, s.uncached), (200, 30, 7)); // its last call
    assert_eq!(s.busy_since, Some((t0 + 50) * 1000));
    assert_eq!((s.started, s.start.len(), s.total), (t0, 5, 371 + 11));
    // What each call read (input + cache) and wrote (output + reasoning), from when it was made
    // to when it completed; the running one has no end yet. Then the subagent's.
    let call = |sent: u64, done: u64, input, output| Call {
        sent: t0 * 1000 + sent,
        done: t0 * 1000 + done,
        input,
        output,
    };
    assert_eq!(
        s.calls,
        [
            call(1000, 9000, 125, 7),
            call(60_000, 60_000, 237, 1),
            call(6000, 7000, 3, 2)
        ]
    );
    assert_eq!(s.lines, Some((4, 2)));
    let tools: Vec<_> = s
        .tools
        .iter()
        .map(|t| (t.id.as_str(), t.arg.as_str(), t.done.map(|d| d.1), t.sub))
        .collect();
    assert_eq!(
        tools,
        [
            ("c2", "*** Begin Patch", Some(false), false),
            ("c3", "d.rs", Some(true), false),
            ("k1", "e.txt", Some(false), true),
            ("k2", "fn main", Some(false), true),
            ("c4", "cargo test -q", None, false),
        ]
    );
    let changes: Vec<_> = s
        .changes
        .iter()
        .map(|c| {
            (
                c.at - t0,
                c.path.to_str().unwrap(),
                c.added,
                c.removed,
                c.read,
            )
        })
        .collect();
    assert_eq!(
        changes,
        [
            (2, "/p/src/a.rs", 0, 0, true),
            (4, "/p/src/b.rs", 1, 2, false),
            (4, "/p/c.md", 1, 0, false),
            (6, "/p/e.txt", 2, 0, false),
        ]
    );
}
