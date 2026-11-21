//! Chat history: each document's conversations with the agent in the Agent
//! panel, kept on this computer (in ZigZag's settings folder, `chats/`),
//! never in the design file. A design carries only a random id
//! (`Settings::chats`) saying which chats are its.
//!
//! Plain files, as Claude Code and Codex keep theirs: `index.json` lists
//! every chat (its document, title, agent, model, session, when), and each
//! chat's lines are a JSON Lines file of their own, appended to as they're
//! said. Nothing is read until it's wanted (the index when a document comes
//! forward, a chat when it's opened), and nothing is written on the UI
//! thread: writes go to a thread of their own, in order.
//!
//! What's kept is what the panel shows (what was asked, what the agent
//! said, a line per step), not previews or tools' whole answers, so a long
//! chat is a few hundred kilobytes at most.

use crate::agent::{AgentKind, Entry};
use crate::app::Studio;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The chats kept for one document; older ones go.
pub const PER_DOCUMENT: usize = 20;
/// The chats kept in all.
pub const IN_ALL: usize = 400;
/// A document's last chat this recent comes back with it; an older one is
/// offered instead.
pub const RECENT_SECS: u64 = 24 * 3600;
/// The most of a chat's file read back (its end), for one that's grown huge.
const MOST_READ: u64 = 1 << 20;

/// One chat, as the index lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct Meta {
    pub id: String,
    /// The document's chats id (see `Settings::chats`).
    pub doc: String,
    /// Its file when the chat was last kept (to find its chats if a copy
    /// of it lost the id).
    pub path: Option<String>,
    /// Its first prompt, cut short.
    pub title: String,
    pub agent: AgentKind,
    pub model: Option<String>,
    /// The agent's session, to continue it.
    pub session: Option<String>,
    pub created: u64,
    pub updated: u64,
    pub entries: usize,
}

impl Meta {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id, "doc": self.doc, "path": self.path, "title": self.title,
            "agent": self.agent.key(), "model": self.model, "session": self.session,
            "created": self.created, "updated": self.updated, "entries": self.entries,
        })
    }

    fn from_json(v: &Value) -> Option<Meta> {
        let s = |k: &str| v[k].as_str().map(str::to_string);
        let id = s("id").filter(|i| zigzag::document::is_chats_id(i))?;
        Some(Meta {
            id,
            doc: s("doc")?,
            path: s("path"),
            title: s("title").unwrap_or_default(),
            agent: AgentKind::from_key(v["agent"].as_str()?)?,
            model: s("model"),
            session: s("session"),
            created: v["created"].as_u64().unwrap_or(0),
            updated: v["updated"].as_u64().unwrap_or(0),
            entries: v["entries"].as_u64().unwrap_or(0) as usize,
        })
    }

    /// When it was last used, said as a person would.
    pub fn ago(&self) -> String {
        let s = now().saturating_sub(self.updated);
        match s {
            0..60 => "just now".into(),
            60..3600 => format!("{} min ago", s / 60),
            3600..86400 => format!("{} h ago", s / 3600),
            86400..172800 => "yesterday".into(),
            _ => format!("{} days ago", s / 86400),
        }
    }
}

/// Seconds since 1970.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A fresh random-looking id (16 hex digits).
pub fn new_id() -> String {
    use std::hash::{Hash, Hasher};
    static N: AtomicUsize = AtomicUsize::new(0);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        .hash(&mut h);
    std::process::id().hash(&mut h);
    N.fetch_add(1, Ordering::Relaxed).hash(&mut h);
    format!("{:016x}", h.finish())
}

/// A transcript line as it's kept.
pub fn entry_json(e: &Entry) -> Value {
    match e {
        Entry::You(t) => json!({ "you": t }),
        Entry::Said(t) => json!({ "said": t }),
        Entry::Tool(name, args, ok) => json!({ "tool": name, "args": args, "ok": ok }),
        Entry::Error(t) => json!({ "error": t }),
        Entry::Done(t) => json!({ "done": t }),
        Entry::Notes(n) => json!({ "notes": n }),
        Entry::Info(t) => json!({ "info": t }),
        Entry::Images(p) => json!({ "images": p }),
        Entry::Choices {
            title,
            question,
            choices,
            applied,
        } => json!({
            "choices": title,
            "question": question,
            "items": choices.iter().map(|c| json!({ "label": c.label, "detail": c.detail, "steps": c.steps })).collect::<Vec<_>>(),
            "applied": applied,
        }),
    }
}

/// A kept line read back (`None` for one this version doesn't know).
pub fn entry_from(v: &Value) -> Option<Entry> {
    let s = |k: &str| v[k].as_str().map(str::to_string);
    Some(if let Some(t) = s("you") {
        Entry::You(t)
    } else if let Some(t) = s("said") {
        Entry::Said(t)
    } else if let Some(t) = s("tool") {
        // (A step still running when it was kept ran no further.)
        Entry::Tool(
            t,
            s("args").unwrap_or_default(),
            Some(v["ok"].as_bool().unwrap_or(false)),
        )
    } else if let Some(t) = s("error") {
        Entry::Error(t)
    } else if let Some(t) = s("done") {
        Entry::Done(t)
    } else if let Some(n) = v["notes"].as_array() {
        Entry::Notes(
            n.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect(),
        )
    } else if let Some(t) = s("choices") {
        // (An earlier chat's card comes back closed: what it offered may
        // no longer fit the design.)
        Entry::Choices {
            title: t,
            question: s("question").unwrap_or_default(),
            choices: v["items"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| crate::agent::Choice {
                    label: c["label"].as_str().unwrap_or("").to_string(),
                    detail: c["detail"].as_str().unwrap_or("").to_string(),
                    steps: c["steps"].clone(),
                    picked: false,
                })
                .collect(),
            applied: Some(
                v["applied"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect(),
            ),
        }
    } else if let Some(p) = v["images"].as_array() {
        Entry::Images(
            p.iter()
                .filter_map(|x| x.as_str().map(PathBuf::from))
                .collect(),
        )
    } else {
        Entry::Info(s("info")?)
    })
}

/// Something for the writer to do.
enum Op {
    Append(PathBuf, String),
    /// The whole file (written beside it, then moved over it).
    Write(PathBuf, String),
    /// A chat's file, and the images its lines name.
    RemoveChat(PathBuf),
}

/// The chats on disk: the index (read once, when first wanted) and a
/// writer thread.
#[derive(Default)]
pub struct Store {
    dir: Option<PathBuf>,
    index: Option<Vec<Meta>>,
    tx: Option<Sender<Op>>,
    /// Writes sent and not yet done (a read waits for them).
    pending: Arc<AtomicUsize>,
    /// Chats this app has let go of: another running copy's index may
    /// still list them, and they aren't taken back from it.
    removed: Vec<String>,
}

impl Store {
    /// The chats kept in `dir` (`None`: kept nowhere).
    pub fn at(dir: Option<PathBuf>) -> Store {
        Store {
            dir,
            ..Default::default()
        }
    }

    /// The person's own: ZigZag's settings folder (nowhere in tests, which
    /// say where if they want one).
    pub fn usual() -> Store {
        Store::at(if cfg!(test) {
            None
        } else {
            crate::dock::config_file("chats")
        })
    }

    pub fn kept(&self) -> bool {
        self.dir.is_some()
    }

    /// The folder chats are kept in, if they are.
    pub fn dir(&self) -> Option<&std::path::Path> {
        self.dir.as_deref()
    }

    fn send(&mut self, op: Op) {
        let Some(dir) = self.dir.clone() else { return };
        if self.tx.is_none() {
            let (tx, rx) = mpsc::channel::<Op>();
            let pending = self.pending.clone();
            std::thread::spawn(move || {
                let _ = std::fs::create_dir_all(&dir);
                for op in rx {
                    match op {
                        Op::Append(p, s) => {
                            use std::io::Write;
                            if let Ok(mut f) = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(&p)
                            {
                                let _ = f.write_all(s.as_bytes());
                            }
                        }
                        Op::Write(p, s) => {
                            let tmp = p.with_extension("tmp");
                            if std::fs::write(&tmp, s).is_ok() {
                                let _ = std::fs::rename(&tmp, &p);
                            }
                        }
                        Op::RemoveChat(p) => {
                            for l in std::fs::read_to_string(&p).unwrap_or_default().lines() {
                                if let Ok(v) = serde_json::from_str::<Value>(l)
                                    && let Some(Entry::Images(images)) = entry_from(&v)
                                {
                                    for i in images {
                                        let _ = std::fs::remove_file(i);
                                    }
                                }
                            }
                            let _ = std::fs::remove_file(p);
                        }
                    }
                    pending.fetch_sub(1, Ordering::AcqRel);
                }
            });
            self.tx = Some(tx);
        }
        self.pending.fetch_add(1, Ordering::AcqRel);
        if let Some(tx) = &self.tx
            && tx.send(op).is_err()
        {
            self.pending.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Wait (a little) for what's been sent to be written.
    pub fn flush(&self) {
        let until = Instant::now() + Duration::from_millis(500);
        while self.pending.load(Ordering::Acquire) > 0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn file(&self, id: &str) -> Option<PathBuf> {
        Some(self.dir.as_ref()?.join(format!("{id}.jsonl")))
    }

    /// Every chat kept (read from disk the first time).
    pub fn index(&mut self) -> &mut Vec<Meta> {
        if self.index.is_none() {
            let list = self
                .dir
                .as_ref()
                .and_then(|d| std::fs::read_to_string(d.join("index.json")).ok())
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .and_then(|v| {
                    v.as_array()
                        .map(|a| a.iter().filter_map(Meta::from_json).collect())
                })
                .unwrap_or_default();
            self.index = Some(list);
        }
        self.index.get_or_insert_with(Vec::new)
    }

    /// A document's chats, the latest first: by its chats id, or (`path`)
    /// by its file for a copy of it that's lost the id.
    pub fn for_doc(&mut self, doc: Option<&str>, path: Option<&str>) -> Vec<Meta> {
        let mut v: Vec<Meta> = self
            .index()
            .iter()
            .filter(|m| {
                doc.is_some_and(|d| m.doc == d)
                    || (doc.is_none() && path.is_some() && m.path.as_deref() == path)
            })
            .cloned()
            .collect();
        v.sort_by_key(|m| std::cmp::Reverse(m.updated));
        v
    }

    pub fn get(&mut self, id: &str) -> Option<Meta> {
        self.index().iter().find(|m| m.id == id).cloned()
    }

    fn write_index(&mut self) {
        let Some(dir) = self.dir.clone() else { return };
        let text = Value::Array(self.index().iter().map(Meta::to_json).collect()).to_string();
        self.send(Op::Write(dir.join("index.json"), text));
    }

    /// Change `m` in the index in memory only: written with the next
    /// [`Store::put`] (lines added while the agent works don't each
    /// rewrite the index).
    pub fn touch(&mut self, m: Meta) {
        if let Some(x) = self.index().iter_mut().find(|x| x.id == m.id) {
            *x = m;
        }
    }

    /// Before the index is written over: the chats another running copy
    /// of the app has kept since it was read (two can be open on the same
    /// settings; each would otherwise drop the other's from the index,
    /// leaving their files with nothing pointing at them).
    fn take_in_others(&mut self) {
        let Some(dir) = self.dir.clone() else { return };
        // (What this copy has sent is written first.)
        self.flush();
        let on_disk: Vec<Meta> = std::fs::read_to_string(dir.join("index.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| {
                v.as_array()
                    .map(|a| a.iter().filter_map(Meta::from_json).collect())
            })
            .unwrap_or_default();
        let removed = self.removed.clone();
        let list = self.index();
        for m in on_disk {
            if removed.contains(&m.id) {
                continue;
            }
            match list.iter_mut().find(|x| x.id == m.id) {
                Some(x) if m.updated > x.updated => *x = m,
                Some(_) => {}
                None => list.push(m),
            }
        }
    }

    /// Keep `m` in the index (new or changed), letting the oldest go past
    /// the limits.
    pub fn put(&mut self, m: Meta) {
        let doc = m.doc.clone();
        self.take_in_others();
        let list = self.index();
        match list.iter_mut().find(|x| x.id == m.id) {
            Some(x) => *x = m,
            None => list.push(m),
        }
        let mut gone: Vec<String> = Vec::new();
        let mut mine: Vec<&Meta> = list.iter().filter(|x| x.doc == doc).collect();
        if mine.len() > PER_DOCUMENT {
            mine.sort_by_key(|m| std::cmp::Reverse(m.updated));
            gone.extend(mine[PER_DOCUMENT..].iter().map(|x| x.id.clone()));
        }
        if list.len() - gone.len() > IN_ALL {
            let mut all: Vec<&Meta> = list.iter().filter(|x| !gone.contains(&x.id)).collect();
            all.sort_by_key(|m| std::cmp::Reverse(m.updated));
            gone.extend(all[IN_ALL..].iter().map(|x| x.id.clone()));
        }
        list.retain(|x| !gone.contains(&x.id));
        self.removed.extend(gone.iter().cloned());
        for id in gone {
            if let Some(f) = self.file(&id) {
                self.send(Op::RemoveChat(f));
            }
        }
        self.write_index();
    }

    /// Add lines to chat `id`'s file.
    pub fn append(&mut self, id: &str, entries: &[Entry]) {
        let Some(f) = self.file(id) else { return };
        let mut text = String::new();
        for e in entries {
            text.push_str(&entry_json(e).to_string());
            text.push('\n');
        }
        if !text.is_empty() {
            self.send(Op::Append(f, text));
        }
    }

    /// Chat `id`'s lines (its last megabyte, for one that's grown huge).
    pub fn read(&self, id: &str) -> Vec<Entry> {
        self.flush();
        let Some(f) = self.file(id) else {
            return Vec::new();
        };
        let Ok(bytes) = read_end(&f, MOST_READ) else {
            return Vec::new();
        };
        String::from_utf8_lossy(&bytes)
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter_map(|v| entry_from(&v))
            .collect()
    }

    /// Forget every chat.
    pub fn clear(&mut self) {
        // (All of it: another running copy's too.)
        self.take_in_others();
        let ids: Vec<String> = self.index().drain(..).map(|m| m.id).collect();
        self.removed.extend(ids.iter().cloned());
        for id in ids {
            if let Some(f) = self.file(&id) {
                self.send(Op::RemoveChat(f));
            }
        }
        self.write_index();
    }
}

/// The last `most` bytes of a file, from the start of a line.
fn read_end(f: &std::path::Path, most: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(f)?;
    let len = file.metadata()?.len();
    if len <= most {
        let mut v = Vec::new();
        file.read_to_end(&mut v)?;
        return Ok(v);
    }
    file.seek(SeekFrom::Start(len - most))?;
    let mut v = Vec::new();
    file.read_to_end(&mut v)?;
    let first = v.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1);
    Ok(v.split_off(first))
}

impl Studio {
    /// The id the document in front keeps its chats under, given it now if
    /// it has none (a copy of one whose file had chats takes theirs). Set
    /// on the document without counting as an edit, and again if undoing
    /// to before it was given took it away.
    pub(crate) fn chats_id(&mut self) -> String {
        let id = match (self.doc.settings.chats.clone(), self.agent.doc_key.clone()) {
            (Some(id), _) => id,
            (None, Some(id)) => id,
            (None, None) => {
                let path = self.doc.path.as_ref().map(|p| p.display().to_string());
                self.agent
                    .store
                    .for_doc(None, path.as_deref())
                    .first()
                    .map(|m| m.doc.clone())
                    .unwrap_or_else(new_id)
            }
        };
        self.agent.doc_key = Some(id.clone());
        if self.doc.settings.chats.as_deref() != Some(&id) {
            self.doc.settings.chats = Some(id.clone());
            self.stable.settings.chats = Some(id.clone());
        }
        id
    }

    /// Write down what's new in the chat in front: its settled lines (a
    /// step still running is written once it's done), and its entry in the
    /// index. A chat starts being kept with its first prompt.
    pub(crate) fn keep_chat(&mut self) {
        if !self.prefs.agent_history || !self.agent.store.kept() {
            return;
        }
        let a = &mut self.agent;
        if a.log.len() < a.hist_saved {
            // (A new chat began in its place.)
            a.hist_id = None;
            a.hist_saved = 0;
        }
        // (Every frame: only the lines not yet written are looked at, and
        // the index only when there's something to write.)
        let settled = a.hist_saved
            + a.log[a.hist_saved..]
                .iter()
                .position(|e| matches!(e, Entry::Tool(_, _, None)))
                .unwrap_or(a.log.len() - a.hist_saved);
        let new = settled > a.hist_saved;
        let session_moved = a.hist_id.is_some() && a.hist_session != a.session;
        if !new && !session_moved {
            return;
        }
        let meta = a.hist_id.as_ref().and_then(|id| a.store.get(id));
        let begun = meta.is_none();
        let mut meta = match meta {
            Some(m) => m,
            None => {
                let Some(title) = a.log[..settled].iter().find_map(|e| match e {
                    Entry::You(t) => Some(t.clone()),
                    Entry::Images(_) => Some("Image".to_string()),
                    // (Sent with canvas notes alone: the first of them.)
                    Entry::Notes(n) => n.first().cloned(),
                    _ => None,
                }) else {
                    return;
                };
                // (Only the document in front is given an id; a chat kept
                // for one at the back already has its.)
                let doc = match self.agent.doc_key.clone() {
                    Some(d) => d,
                    None => self.chats_id(),
                };
                let a = &self.agent;
                let title: String = title
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect();
                let m = Meta {
                    id: new_id(),
                    doc,
                    path: self.doc.path.as_ref().map(|p| p.display().to_string()),
                    title,
                    agent: a.kind,
                    model: a.model().map(str::to_string),
                    session: None,
                    created: now(),
                    updated: now(),
                    entries: 0,
                };
                self.agent.hist_id = Some(m.id.clone());
                m
            }
        };
        let a = &mut self.agent;
        if new {
            let lines: Vec<Entry> = a.log[a.hist_saved..settled].to_vec();
            a.store.append(&meta.id, &lines);
            a.hist_saved = settled;
            meta.updated = now();
            meta.entries = settled;
            meta.model = a.model().map(str::to_string);
            // (Whose session it is, while it has one: the agent in use
            // may be another by now.)
            meta.agent = a
                .session_kind()
                .filter(|_| a.session.is_some())
                .unwrap_or(a.kind);
        }
        meta.session = a.session.clone();
        a.hist_session = a.session.clone();
        // The index is written when a chat begins, its session changes or
        // a turn ends; in between, only its own file grows.
        if begun || session_moved || !a.running() {
            a.store.put(meta);
        } else {
            a.store.touch(meta);
        }
    }

    /// When a document comes forward with its chat not yet looked for: its
    /// last chat back if it's recent, else offered (`earlier`).
    pub(crate) fn recall_chat(&mut self) {
        if self.agent.hist_checked {
            return;
        }
        self.agent.hist_checked = true;
        if !self.prefs.agent_history
            || !self.agent.store.kept()
            || !self.agent.log.is_empty()
            || self.agent.running()
        {
            return;
        }
        let doc = self.doc.settings.chats.clone();
        let path = self.doc.path.as_ref().map(|p| p.display().to_string());
        if doc.is_none() && path.is_none() {
            return;
        }
        let Some(last) = self
            .agent
            .store
            .for_doc(doc.as_deref(), path.as_deref())
            .into_iter()
            .next()
        else {
            return;
        };
        // (Open in another tab already — a copy of the same design: one
        // chat isn't carried on from two places.)
        let elsewhere = (0..self.tab_count())
            .filter(|&i| i != self.tabs.active)
            .any(|i| self.tabs.list[i].chat.kept_id() == Some(last.id.as_str()));
        if elsewhere {
            return;
        }
        if now().saturating_sub(last.updated) < RECENT_SECS {
            self.open_chat(&last.id);
        } else {
            self.agent.earlier = Some(last);
        }
    }

    /// The chat in front for kept chat `id` (the one in front is kept
    /// already). Not while the agent is working on it.
    pub(crate) fn open_chat(&mut self, id: &str) {
        if self.agent.running() {
            self.agent.log.push(Entry::Info(
                "The agent is working: open an earlier chat once it's done.".into(),
            ));
            return;
        }
        let Some(m) = self.agent.store.get(id) else {
            return;
        };
        let lines = self.agent.store.read(id);
        self.agent.new_chat();
        // (The chat it replaces takes its before-and-afters with it: they
        // were places in that conversation.)
        let here = self.session.rec;
        self.review.turns.retain(|t| t.outside || t.tab != here);
        let showing = match self.review.showing {
            Some(crate::review::Showing::Before(id) | crate::review::Showing::Compare(id)) => {
                Some(id)
            }
            None => None,
        };
        if showing.is_some_and(|id| self.review.turn(id).is_none()) {
            self.review.showing = None;
        }
        self.agent.log = lines;
        self.agent.hist_id = Some(m.id.clone());
        self.agent.hist_saved = self.agent.log.len();
        self.agent.hist_session = m.session.clone();
        self.agent.hist_checked = true;
        self.agent.doc_key = Some(m.doc.clone());
        self.agent.resume(m.session.clone(), m.agent);
        if m.agent != self.agent.kind {
            self.agent.log.push(Entry::Info(format!(
                "This chat was with {}: {} will start a new session from here.",
                m.agent.name(),
                self.agent.kind.name()
            )));
        }
    }

    /// This document's kept chats, the latest first.
    pub(crate) fn doc_chats(&mut self) -> Vec<Meta> {
        let doc = self
            .doc
            .settings
            .chats
            .clone()
            .or_else(|| self.agent.doc_key.clone());
        let path = self.doc.path.as_ref().map(|p| p.display().to_string());
        self.agent.store.for_doc(doc.as_deref(), path.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn studio_with(dir: &std::path::Path, file: &std::path::Path) -> Studio {
        let mut s = Studio::with_file(Some(file.to_path_buf()), None);
        s.agent.config = None;
        s.agent.store = Store::at(Some(dir.to_path_buf()));
        s.prefs.agent_history = true;
        s
    }

    fn svg(dir: &std::path::Path, name: &str) -> PathBuf {
        let f = dir.join(name);
        std::fs::write(
            &f,
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="40mm" height="40mm" viewBox="0 0 40 40"><rect id="a" width="20" height="20" fill="#c0392b"/></svg>"##,
        )
        .unwrap();
        f
    }

    #[test]
    fn a_designs_chat_is_kept_and_comes_back_with_it() {
        let root = std::env::temp_dir().join(format!("zz-chat-doc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let chats = root.join("chats");
        let bear = svg(&root, "bear.svg");
        let fox = svg(&root, "fox.svg");

        let mut s = studio_with(&chats, &bear);
        s.recall_chat();
        assert!(
            s.agent.log.is_empty() && s.agent.earlier.is_none(),
            "nothing kept yet"
        );
        // A turn: the prompt, a step still running (not kept until it's
        // done), then what the agent said.
        s.agent.log.push(Entry::You("clean up the bear".into()));
        s.agent
            .log
            .push(Entry::Tool("trace_image".into(), String::new(), None));
        s.agent.session = Some("thread-1".into());
        s.keep_chat();
        assert!(s.doc.settings.chats.is_some(), "the design got its id");
        assert!(!s.modified, "and that isn't an edit");
        if let Some(Entry::Tool(_, _, ok)) = s.agent.log.last_mut() {
            *ok = Some(true);
        }
        s.agent.log.push(Entry::Said("Traced it in place.".into()));
        s.keep_chat();
        s.agent.store.flush();
        let kept = s.doc_chats();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].title, "clean up the bear");
        assert_eq!(kept[0].session.as_deref(), Some("thread-1"));
        assert_eq!(s.agent.store.read(&kept[0].id).len(), 3);

        // Opened again (from its file, which never had the id saved into
        // it): the chat comes back, ready to continue.
        let mut again = studio_with(&chats, &bear);
        again.recall_chat();
        assert_eq!(again.agent.log.len(), 3);
        assert_eq!(again.agent.session.as_deref(), Some("thread-1"));
        // Another design has none of it.
        let mut other = studio_with(&chats, &fox);
        other.recall_chat();
        assert!(other.agent.log.is_empty());
        assert!(other.doc_chats().is_empty());

        // A day later it's offered instead of opened, and opens on asking.
        let mut m = kept[0].clone();
        m.updated -= RECENT_SECS + 60;
        again.agent.store.put(m.clone());
        let mut later = studio_with(&chats, &bear);
        later.agent.store.put(m.clone());
        later.recall_chat();
        assert!(later.agent.log.is_empty(), "not opened by itself");
        assert_eq!(
            later.agent.earlier.as_ref().map(|e| e.id.clone()),
            Some(m.id.clone())
        );
        let ctx = eframe::egui::Context::default();
        later.run_slash(&format!("/history {}", m.id), &ctx);
        assert_eq!(later.agent.log.len(), 3, "/history opens it");
        assert_eq!(later.agent.hist_id.as_deref(), Some(m.id.as_str()));

        // A new chat is a chat of its own; the first stays kept.
        later.agent.new_chat();
        later.agent.log.push(Entry::You("now a fox".into()));
        later.keep_chat();
        later.agent.store.flush();
        assert_eq!(later.doc_chats().len(), 2);
        assert_eq!(later.doc_chats()[0].title, "now a fox", "latest first");

        // Cleared: nothing kept, and what's on screen isn't written back.
        later.agent.store.clear();
        later.agent.forget_kept();
        later.keep_chat();
        later.agent.store.flush();
        assert!(later.doc_chats().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// How long history takes where it matters (`--ignored`; numbers on
    /// stderr): per frame, on the UI thread, and opening a long chat.
    #[test]
    #[ignore]
    fn history_speed() {
        let ms = |t: Instant| t.elapsed().as_secs_f64() * 1000.0;
        let root = std::env::temp_dir().join(format!("zz-chat-speed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("chats");
        let mut store = Store::at(Some(dir.clone()));
        // A heavy user: 300 chats over 60 designs, one 1,000 lines long.
        let long: Vec<Entry> = (0..1000)
            .map(|k| {
                if k % 3 == 0 {
                    Entry::Said(
                        "I'm refining the bear's silhouette and paws, then tracing it back. "
                            .repeat(3),
                    )
                } else {
                    Entry::Tool(
                        "set_object".into(),
                        "object: Bear body, kind: fill".into(),
                        Some(true),
                    )
                }
            })
            .collect();
        for k in 0..300u64 {
            let id = format!("c{k:04}");
            if k == 7 {
                store.append(&id, &long);
            }
            store.put(Meta {
                id,
                doc: format!("d{:03}", k % 60),
                path: None,
                title: "make the bear more detailed with shading".into(),
                agent: AgentKind::Codex,
                model: Some("gpt-6.2-astra".into()),
                session: Some("01a1f98a-f307-7ef3-83ee-77db4a809738".into()),
                created: k,
                updated: k,
                entries: 3,
            });
        }
        store.flush();
        let t = Instant::now();
        let mut fresh = Store::at(Some(dir.clone()));
        let n = fresh.index().len();
        eprintln!("read the index ({n} chats): {:.2} ms", ms(t));
        let t = Instant::now();
        let mine = fresh.for_doc(Some("d007"), None);
        eprintln!("one design's chats ({}): {:.3} ms", mine.len(), ms(t));
        let t = Instant::now();
        let lines = fresh.read("c0007");
        eprintln!("open a {}-line chat: {:.2} ms", lines.len(), ms(t));
        let t = Instant::now();
        let m = fresh.get("c0007").unwrap();
        fresh.put(m);
        eprintln!(
            "keep a turn (index update, on the UI thread): {:.3} ms",
            ms(t)
        );
        // Per frame, nothing new: the studio's own check.
        let file = svg(&root, "bear.svg");
        let mut s = studio_with(&dir, &file);
        s.agent.log = long.clone();
        s.agent.log.insert(0, Entry::You("a bear".into()));
        s.keep_chat();
        s.agent.store.flush();
        let t = Instant::now();
        for _ in 0..1000 {
            s.keep_chat();
            s.recall_chat();
        }
        eprintln!(
            "per frame, idle (1,001-line chat): {:.4} ms",
            ms(t) / 1000.0
        );
        let t = Instant::now();
        let mut again = studio_with(&dir, &file);
        again.recall_chat();
        eprintln!(
            "a design comes forward and its chat comes back ({} lines): {:.2} ms",
            again.agent.log.len(),
            ms(t)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn lines_go_to_disk_and_come_back() {
        let dir = std::env::temp_dir().join(format!("zz-chats-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = Store::at(Some(dir.clone()));
        let lines = vec![
            Entry::You("a fox".into()),
            Entry::Tool("add_svg".into(), "svg: …".into(), Some(true)),
            Entry::Notes(vec!["1: the ear".into()]),
            Entry::Said("Done.".into()),
            Entry::Info("Model: opus".into()),
            Entry::Done("2 turns".into()),
        ];
        s.append("abc123", &lines);
        let m = Meta {
            id: "abc123".into(),
            doc: "doc1".into(),
            path: None,
            title: "a fox".into(),
            agent: AgentKind::Codex,
            model: None,
            session: Some("s1".into()),
            created: now(),
            updated: now(),
            entries: lines.len(),
        };
        s.put(m.clone());
        s.flush();
        assert_eq!(s.read("abc123"), lines);
        // A new store reads the index from disk.
        let mut again = Store::at(Some(dir.clone()));
        assert_eq!(again.for_doc(Some("doc1"), None), [m]);
        assert!(again.for_doc(Some("doc2"), None).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_document_keeps_its_last_twenty() {
        let dir = std::env::temp_dir().join(format!("zz-chats-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = Store::at(Some(dir.clone()));
        for k in 0..25u64 {
            let id = format!("c{k:02}");
            s.append(&id, &[Entry::You(format!("prompt {k}"))]);
            s.put(Meta {
                id,
                doc: "d".into(),
                path: None,
                title: String::new(),
                agent: AgentKind::Claude,
                model: None,
                session: None,
                created: k,
                updated: k,
                entries: 1,
            });
        }
        s.flush();
        let kept = s.for_doc(Some("d"), None);
        assert_eq!(kept.len(), PER_DOCUMENT);
        assert_eq!(kept[0].id, "c24", "latest first");
        assert!(!dir.join("c00.jsonl").exists(), "the oldest file went too");
        assert!(dir.join("c24.jsonl").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_chat_let_go_takes_its_images_with_it() {
        let dir = std::env::temp_dir().join(format!("zz-chats-img-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("pic.png");
        std::fs::write(&image, b"png").unwrap();
        let mut s = Store::at(Some(dir.clone()));
        let lines = vec![
            Entry::You("this logo".into()),
            Entry::Images(vec![image.clone()]),
        ];
        s.append("old", &lines);
        let meta = |id: &str, t: u64| Meta {
            id: id.into(),
            doc: "d".into(),
            path: None,
            title: String::new(),
            agent: AgentKind::Codex,
            model: None,
            session: None,
            created: t,
            updated: t,
            entries: 2,
        };
        s.put(meta("old", 0));
        s.flush();
        assert_eq!(s.read("old"), lines, "image lines come back");
        for k in 1..=PER_DOCUMENT as u64 {
            s.put(meta(&format!("n{k:02}"), k));
        }
        s.flush();
        assert!(s.get("old").is_none());
        assert!(!image.exists(), "its image went with it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_end_of_a_huge_chat_is_read_from_a_line_start() {
        let dir = std::env::temp_dir().join(format!("zz-chats-end-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("x.jsonl");
        let line = format!("{}\n", json!({ "said": "x".repeat(100) }));
        std::fs::write(&f, line.repeat(50)).unwrap();
        let end = read_end(&f, 300).unwrap();
        let text = String::from_utf8(end).unwrap();
        assert!(
            text.lines()
                .all(|l| serde_json::from_str::<Value>(l).is_ok())
        );
        assert!(!text.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
