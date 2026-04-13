use anyhow::Result;
use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const RECENT_TOOLS_CAP: usize = 8;

#[derive(Debug)]
pub struct ClaudeContext {
    pub session_path: PathBuf,
    pub session_cwd: PathBuf,
    pub last_user: Option<String>,
    pub last_assistant: Option<String>,
    /// Name of a tool_use that hasn't seen its matching tool_result yet.
    pub active_tool: Option<String>,
    /// Short description of what the active tool is operating on (command,
    /// file path, pattern, etc.) — derived from the tool's `input` object.
    pub active_tool_target: Option<String>,
    pub git_branch: Option<String>,
    /// Count of human turns (user messages that aren't just tool_results).
    pub turn_count: u32,
    /// Histogram of tools called this session.
    pub tool_counts: BTreeMap<String, u32>,
    /// The first real user prompt of the session — usually describes the goal.
    pub topic: Option<String>,
    /// Rolling window of recent tool_use events, oldest first.
    pub recent_tools: VecDeque<(String, Option<String>)>,
    last_offset: u64,
    buffer: String,
}

impl ClaudeContext {
    pub fn new(session_path: PathBuf, session_cwd: PathBuf) -> Self {
        Self {
            session_path,
            session_cwd,
            last_user: None,
            last_assistant: None,
            active_tool: None,
            active_tool_target: None,
            git_branch: None,
            turn_count: 0,
            tool_counts: BTreeMap::new(),
            topic: None,
            recent_tools: VecDeque::with_capacity(RECENT_TOOLS_CAP),
            last_offset: 0,
            buffer: String::new(),
        }
    }

pub fn tool_count_total(&self) -> u32 {
        self.tool_counts.values().sum()
    }

    /// Top-N tools by usage, formatted like `Bash×3 Edit×4 Read×1`.
    pub fn top_tools(&self, n: usize) -> String {
        let mut items: Vec<(&String, &u32)> = self.tool_counts.iter().collect();
        items.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        items
            .iter()
            .take(n)
            .map(|(name, count)| format!("{}×{}", name, count))
            .collect::<Vec<_>>()
            .join(" ")
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

        if let Some(branch) = val.get("gitBranch").and_then(|v| v.as_str()) {
            if !branch.is_empty() {
                self.git_branch = Some(branch.to_string());
            }
        }

        let ty = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match ty {
            "user" => {
                let content = val.get("message").and_then(|m| m.get("content"));
                // Synthetic "user" messages injected after tool calls carry
                // tool_result blocks — don't count those as human turns.
                if contains_tool_result(content) {
                    self.active_tool = None;
                    self.active_tool_target = None;
                    return;
                }
                if let Some(text) = extract_text(content) {
                    if self.topic.is_none() {
                        self.topic = Some(text.clone());
                    }
                    self.last_user = Some(text);
                    self.turn_count += 1;
                    self.active_tool = None;
                    self.active_tool_target = None;
                }
            }
            "assistant" => {
                let content = val.get("message").and_then(|m| m.get("content"));
                if let Some(arr) = content.and_then(|c| c.as_array()) {
                    for block in arr {
                        let bty = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        match bty {
                            "text" => {
                                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                                    if !t.trim().is_empty() {
                                        self.last_assistant = Some(t.to_string());
                                    }
                                }
                            }
                            "tool_use" => {
                                let name = block
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?")
                                    .to_string();
                                let target = block
                                    .get("input")
                                    .map(|input| format_tool_target(&name, input))
                                    .filter(|s| !s.is_empty());
                                *self.tool_counts.entry(name.clone()).or_insert(0) += 1;
                                if self.recent_tools.len() == RECENT_TOOLS_CAP {
                                    self.recent_tools.pop_front();
                                }
                                self.recent_tools.push_back((name.clone(), target.clone()));
                                self.active_tool = Some(name);
                                self.active_tool_target = target;
                            }
                            _ => {}
                        }
                    }
                }
            }
            "tool_result" => {
                self.active_tool = None;
                self.active_tool_target = None;
            }
            _ => {}
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

/// Extract a short, human-readable "target" from a tool_use block's input:
/// for Bash → the command, for file tools → the basename, etc.
fn format_tool_target(name: &str, input: &serde_json::Value) -> String {
    let as_str = |key: &str| input.get(key).and_then(|v| v.as_str()).unwrap_or("");
    match name {
        "Bash" => as_str("command").to_string(),
        "Read" | "Edit" | "Write" | "NotebookEdit" => {
            let p = as_str("file_path");
            basename_or(p)
        }
        "Grep" => {
            let pat = as_str("pattern");
            let path = as_str("path");
            if path.is_empty() {
                pat.to_string()
            } else {
                format!("{} in {}", pat, basename_or(path))
            }
        }
        "Glob" => as_str("pattern").to_string(),
        "WebFetch" => as_str("url").to_string(),
        "WebSearch" => as_str("query").to_string(),
        "Agent" => {
            let desc = as_str("description");
            let st = as_str("subagent_type");
            if !desc.is_empty() && !st.is_empty() {
                format!("{} ({})", desc, st)
            } else if !desc.is_empty() {
                desc.to_string()
            } else {
                st.to_string()
            }
        }
        "Skill" => {
            let skill = as_str("skill");
            let args = as_str("args");
            if args.is_empty() {
                skill.to_string()
            } else {
                format!("{} {}", skill, args)
            }
        }
        _ => String::new(),
    }
}

fn basename_or(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| path.to_string())
}

/// Convert a cwd like `/Users/foo/Projects/Bar` to the slug Claude Code uses
/// for its project directory name: `-Users-foo-Projects-Bar`.
pub fn slugify_cwd(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    s.replace('/', "-")
}

/// Find the newest `.jsonl` session file under `~/.claude/projects/<slug>/`.
pub fn find_session(home: &Path, cwd: &Path) -> Option<PathBuf> {
    let slug = slugify_cwd(cwd);
    let dir = home.join(".claude").join("projects").join(slug);
    let entries = std::fs::read_dir(&dir).ok()?;
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        match &newest {
            Some((best, _)) if *best >= mtime => {}
            _ => newest = Some((mtime, path)),
        }
    }
    newest.map(|(_, p)| p)
}
