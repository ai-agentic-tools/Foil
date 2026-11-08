//! The audit log: one JSON object per line, appended as things happen.
//!
//! A run records how it was set up (`start`), every network request the
//! proxy allowed or blocked (`net`), how it ended (`end`), and what became
//! of its branch (`review`, `commit`, `abort`). Branch runs keep their log
//! at `$FOIL_HOME/audit/<branch>.jsonl`, outside anything the run can see,
//! so the run can't edit its own record. `foil audit` reads it back.
//!
//! Each line goes out in one `write` on a file opened with `O_APPEND`, so
//! foil and its proxy (another process) can add lines to the same log
//! without tearing each other's.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

/// An open audit log.
pub struct Log {
    file: File,
}

impl Log {
    /// Open (or create, readable by its owner only) the log at `path`.
    pub fn open(path: &Path) -> io::Result<Log> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new().create(true).append(true).mode(0o600).open(path)?;
        Ok(Log { file })
    }

    /// Append event `kind` with `fields` (an object) and a timestamp.
    pub fn event(&self, kind: &str, fields: Value) {
        let mut line = Map::new();
        line.insert("ts".into(), Value::String(now()));
        line.insert("event".into(), Value::String(kind.into()));
        if let Value::Object(more) = fields {
            line.extend(more);
        }
        let mut bytes = Value::Object(line).to_string().into_bytes();
        bytes.push(b'\n');
        // One write for the whole line. A log that can't be written must not
        // stop the run (the run's own output says what happened, too).
        let _ = (&self.file).write_all(&bytes);
    }
}

/// Where branch `name` keeps its log.
pub fn path_for(name: &str) -> io::Result<PathBuf> {
    Ok(crate::branch::home()?.join("audit").join(format!("{name}.jsonl")))
}

/// Append one event to branch `name`'s log, if it has one.
pub fn note(name: &str, kind: &str, fields: Value) {
    if let Ok(path) = path_for(name) {
        if path.exists() {
            if let Ok(log) = Log::open(&path) {
                log.event(kind, fields);
            }
        }
    }
}

/// UTC time as `2026-11-08T10:15:00.123Z`.
pub fn now() -> String {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let (y, m, day) = crate::agent::civil((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        d.subsec_millis()
    )
}

/// What `foil audit` prints: a summary of one log.
#[derive(Default, Debug)]
pub struct Summary {
    pub start: Option<Value>,
    pub end: Option<Value>,
    /// `host:port` -> requests allowed.
    pub allowed: Vec<(String, usize)>,
    /// `host:port`, why -> requests blocked.
    pub blocked: Vec<(String, String, usize)>,
    /// The last word on the branch: review, commit or abort events.
    pub outcomes: Vec<Value>,
    /// Lines that were not JSON objects (a torn or edited log).
    pub bad_lines: usize,
}

pub fn summarize(text: &str) -> Summary {
    let mut s = Summary::default();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(Value::Object(ev)) = serde_json::from_str::<Value>(line) else {
            s.bad_lines += 1;
            continue;
        };
        let kind = ev.get("event").and_then(Value::as_str).unwrap_or("").to_owned();
        let ev = Value::Object(ev);
        match kind.as_str() {
            "start" => s.start = Some(ev),
            "end" => s.end = Some(ev),
            "net" => {
                let host = ev.get("host").and_then(Value::as_str).unwrap_or("?");
                let port = ev.get("port").and_then(Value::as_u64).unwrap_or(0);
                let target = format!("{host}:{port}");
                if ev.get("verdict").and_then(Value::as_str) == Some("allowed") {
                    match s.allowed.iter_mut().find(|(t, _)| *t == target) {
                        Some(entry) => entry.1 += 1,
                        None => s.allowed.push((target, 1)),
                    }
                } else {
                    let why = ev.get("reason").and_then(Value::as_str).unwrap_or("blocked").to_owned();
                    match s.blocked.iter_mut().find(|(t, w, _)| *t == target && *w == why) {
                        Some(entry) => entry.2 += 1,
                        None => s.blocked.push((target, why, 1)),
                    }
                }
            }
            "review" | "commit" | "abort" => s.outcomes.push(ev),
            _ => {}
        }
    }
    s.allowed.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    s.blocked.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
    s
}

impl Summary {
    pub fn allowed_total(&self) -> usize {
        self.allowed.iter().map(|a| a.1).sum()
    }

    pub fn blocked_total(&self) -> usize {
        self.blocked.iter().map(|b| b.2).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::summarize;

    #[test]
    fn summary_counts_requests_by_host_and_verdict() {
        let log = r#"{"ts":"t","event":"start","command":["claude"]}
{"ts":"t","event":"net","host":"api.anthropic.com","port":443,"verdict":"allowed"}
{"ts":"t","event":"net","host":"api.anthropic.com","port":443,"verdict":"allowed"}
{"ts":"t","event":"net","host":"evil.test","port":443,"verdict":"blocked","reason":"not in the allowlist"}
not json
{"ts":"t","event":"end","status":0}
{"ts":"t","event":"commit","applied":2}
"#;
        let s = summarize(log);
        assert_eq!(s.allowed, vec![("api.anthropic.com:443".to_owned(), 2)]);
        assert_eq!(s.blocked, vec![("evil.test:443".to_owned(), "not in the allowlist".to_owned(), 1)]);
        assert_eq!(s.bad_lines, 1);
        assert!(s.start.is_some() && s.end.is_some());
        assert_eq!(s.outcomes.len(), 1);
    }

    #[test]
    fn timestamps_look_like_rfc3339() {
        let t = super::now();
        assert_eq!(t.len(), 24, "{t}");
        assert!(t.ends_with('Z') && t.as_bytes()[10] == b'T', "{t}");
    }
}
