use anyhow::Result;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

#[derive(Debug)]
pub struct ClaudeContext {
    pub session_path: PathBuf,
    pub session_cwd: PathBuf,
    /// Start time of the `claude` process this context is attached to. Used
    /// to detect re-invocations: when the user quits and re-runs `claude` in
    /// the same cwd, the new process has a different start_time and we must
    /// rebuild the context against the new JSONL file.
    pub session_started_at: Option<SystemTime>,
    pub last_user: Option<String>,
    last_offset: u64,
    buffer: String,
}

impl ClaudeContext {
    pub fn new(
        session_path: PathBuf,
        session_cwd: PathBuf,
        session_started_at: Option<SystemTime>,
    ) -> Self {
        Self {
            session_path,
            session_cwd,
            session_started_at,
            last_user: None,
            last_offset: 0,
            buffer: String::new(),
        }
    }

    /// Read any new bytes from the session file and parse completed JSONL lines.
    pub fn tick(&mut self) -> Result<()> {
        let mut f = match File::open(&self.session_path) {
            Ok(f) => f,
            Err(_) => return Ok(()),
        };
        let meta = f.metadata()?;
        let len = meta.len();

        // File was truncated or rotated -> rewind.
        if len < self.last_offset {
            self.last_offset = 0;
            self.buffer.clear();
        }
        if len == self.last_offset {
            return Ok(());
        }

        f.seek(SeekFrom::Start(self.last_offset))?;
        let to_read = (len - self.last_offset) as usize;
        let mut new_bytes = vec![0u8; to_read];
        f.read_exact(&mut new_bytes)?;
        self.last_offset = len;
        self.buffer.push_str(&String::from_utf8_lossy(&new_bytes));

        while let Some(nl) = self.buffer.find('\n') {
            let line: String = self.buffer.drain(..=nl).collect();
            let trimmed = line.trim_end();
            if !trimmed.is_empty() {
                self.parse_line(trimmed);
            }
        }
        Ok(())
    }

    fn parse_line(&mut self, line: &str) {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("user") {
            return;
        }
        let content = val.get("message").and_then(|m| m.get("content"));
        // Synthetic "user" messages injected after tool calls carry tool_result
        // blocks; they aren't human input.
        if contains_tool_result(content) {
            return;
        }
        if let Some(text) = extract_text(content) {
            self.last_user = Some(text);
        }
    }
}

fn contains_tool_result(content: Option<&serde_json::Value>) -> bool {
    let Some(arr) = content.and_then(|c| c.as_array()) else {
        return false;
    };
    arr.iter()
        .any(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_result"))
}

fn extract_text(content: Option<&serde_json::Value>) -> Option<String> {
    match content? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(arr) => {
            let mut out = String::new();
            for block in arr {
                if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(t);
                    }
                }
            }
            if out.is_empty() { None } else { Some(out) }
        }
        _ => None,
    }
}

/// Convert a cwd like `/Users/foo/Projects/Bar` to the slug Claude Code uses
/// for its project directory name: `-Users-foo-Projects-Bar`.
pub fn slugify_cwd(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    s.replace('/', "-")
}

/// Find the newest `.jsonl` session file under `~/.claude/projects/<slug>/`.
///
/// When `since` is provided, JSONL files whose mtime predates it by more than
/// a few seconds of slack are ignored. This prevents latching onto an old
/// session file that happens to be lying around when a fresh `claude` process
/// has just started and hasn't written its own JSONL yet. Returning `None`
/// in that window is fine — the caller will retry on the next poll.
pub fn find_session(home: &Path, cwd: &Path, since: Option<SystemTime>) -> Option<PathBuf> {
    const STALE_SLACK: Duration = Duration::from_secs(2);

    let slug = slugify_cwd(cwd);
    let dir = home.join(".claude").join("projects").join(slug);
    let entries = std::fs::read_dir(&dir).ok()?;
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        if let Some(threshold) = since {
            if mtime + STALE_SLACK < threshold {
                continue;
            }
        }
        match &newest {
            Some((best, _)) if *best >= mtime => {}
            _ => newest = Some((mtime, path)),
        }
    }
    newest.map(|(_, p)| p)
}
