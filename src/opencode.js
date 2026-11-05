// cc-top's opencode plugin, installed by `cc-top --setup`. Each second it writes what this opencode
// window has open (its tabs) to <claude config>/cc-top/opencode/<pid>.json, for cc-top to show
// next to the Claude Code sessions. Reads opencode's state only: it never changes a session.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const DIR = path.join(process.env.CLAUDE_CONFIG_DIR || path.join(os.homedir(), ".claude"), "cc-top", "opencode");
const FILE = path.join(DIR, `${process.pid}.json`);
const ACTIVITY = 5; // tool calls kept per session, as in cc-top
const CHANGES = 16; // file reads and edits kept per session

export default {
  id: "cc-top",
  setup(api) {
    if (!api.ui?.tabs) return; // not a full TUI
    fs.mkdirSync(DIR, { recursive: true });
    prune();
    let last = "";
    const write = () => {
      try {
        const text = JSON.stringify({ pid: process.pid, sessions: open(api).map((id) => snapshot(api, id)) });
        if (text === last) return;
        fs.writeFileSync(`${FILE}.tmp`, text);
        fs.renameSync(`${FILE}.tmp`, FILE); // atomic, so cc-top never reads half a file
        last = text;
      } catch {} // try again next second
    };
    write();
    const timer = setInterval(write, 1000);
    return () => {
      clearInterval(timer);
      fs.rmSync(FILE, { force: true });
    };
  },
};

/// Files of windows that are gone without cleaning up (a crash, a kill -9).
function prune() {
  for (const name of fs.readdirSync(DIR)) {
    const pid = parseInt(name);
    try {
      if (pid) process.kill(pid, 0);
    } catch (e) {
      if (e.code === "ESRCH") fs.rmSync(path.join(DIR, name), { force: true });
    }
  }
}

/// The top-level sessions in this window's tabs, and the one on screen. A subagent's counts as its parent.
function open(api) {
  const ids = api.ui.tabs.enabled() ? api.ui.tabs.list().map((t) => t.sessionID) : [];
  const route = api.ui.router.current();
  if (route.type === "session") ids.push(route.sessionID);
  const roots = ids.map((id) => api.data.session.root(id) ?? id);
  return [...new Set(roots)].filter((id) => api.data.session.get(id));
}

const secs = (ms) => Math.floor(ms / 1000);
const read = (t) => (t ? t.input + t.cache.read + t.cache.write : 0);
const wrote = (t) => (t ? t.output + t.reasoning : 0);
const tokens = (t) => read(t) + wrote(t);

function snapshot(api, id) {
  const s = api.data.session.get(id);
  const model = (api.data.location.model.list(s.location) ?? []).find(
    (m) => m.providerID === s.model?.providerID && (m.id === s.model?.id || m.modelID === s.model?.id),
  );
  const variant = s.model?.variant === "default" ? "" : (s.model?.variant ?? "");
  const out = {
    id,
    name: s.title ?? "",
    cwd: s.location?.directory ?? "",
    model: `${model?.name ?? s.model?.id ?? ""} ${variant}`.trim(),
    window: model?.limit?.context ?? 0,
    started: secs(s.time.created),
    start: new Date(s.time.created).toTimeString().slice(0, 5), // local HH:MM
    busy_since: api.data.session.status(id) === "running" ? (s.time.busy ?? null) : null,
    context: null,
    total: 0,
    lines: [0, 0],
    calls: [],
    tools: [],
    changes: [],
  };
  // The session and its subagents.
  for (const member of new Set([id, ...(api.data.session.family(id) ?? [])])) {
    out.total += tokens(api.data.session.get(member)?.tokens);
    for (const m of api.data.session.message.list(member) ?? []) {
      if (m.type !== "assistant") continue;
      if (m.tokens && tokens(m.tokens) > 0) {
        // [sent, done] in unix ms, then what it read and wrote, as cc-top's `Call`.
        out.calls.push([m.time.created, m.time.completed ?? m.time.created, read(m.tokens), wrote(m.tokens)]);
        if (member === id) out.context = { read: m.tokens.cache.read, write: m.tokens.cache.write, input: m.tokens.input };
      }
      for (const t of m.content ?? []) {
        if (t.type !== "tool") continue;
        const done = t.time?.completed && [secs(t.time.completed), t.state?.status !== "completed"];
        out.tools.push({ id: t.id, at: secs(t.time?.created ?? m.time.created), name: t.name, input: strings(t.state?.input), done: done || null, sub: member !== id });
        for (const c of changes(t, done)) {
          out.changes.push(c);
          out.lines[0] += c.added;
          out.lines[1] += c.removed;
        }
      }
    }
  }
  out.tools = out.tools.sort((a, b) => a.at - b.at).slice(-ACTIVITY);
  out.changes = out.changes.sort((a, b) => a.at - b.at).slice(-CHANGES);
  return out;
}

/// The string inputs of a tool call, cut short: cc-top picks the one to show.
function strings(input) {
  return Object.fromEntries(
    Object.entries(input ?? {})
      .filter(([, v]) => typeof v === "string")
      .map(([k, v]) => [k, v.slice(0, 300)]),
  );
}

/// A read when it is called (its file may be an image, or fail), an edit once it went through.
function changes(t, done) {
  const input = t.state?.input ?? {};
  const file = input.filePath ?? input.path;
  if (t.name === "read") return file ? [{ at: secs(t.time?.created ?? 0), path: file, added: 0, removed: 0, read: true }] : [];
  if (!done || done[1]) return [];
  const diff = t.state?.metadata?.diff;
  if (typeof diff === "string") return diffFiles(diff).map((f) => ({ at: done[0], ...f, read: false }));
  if (t.name === "write" && file) {
    const content = input.content ?? "";
    const lines = content.split("\n").length - (content.endsWith("\n") ? 1 : 0);
    return [{ at: done[0], path: file, added: lines, removed: 0, read: false }];
  }
  return [];
}

/// Each `Index: <path>` file of a unified diff, with the lines its hunks add and remove.
function diffFiles(diff) {
  const files = [];
  let hunk = false; // past the `---`/`+++` header of the current file
  for (const line of diff.split("\n")) {
    if (line.startsWith("Index: ")) {
      files.push({ path: line.slice(7), added: 0, removed: 0 });
      hunk = false;
    } else if (line.startsWith("@@")) hunk = files.length > 0;
    else if (hunk && line[0] === "+") files.at(-1).added++;
    else if (hunk && line[0] === "-") files.at(-1).removed++;
  }
  return files;
}
