use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line as GridLine};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::Config as TermConfig;
use alacritty_terminal::vte::ansi::Processor;
use alacritty_terminal::Term;
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::{Child, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use std::path::PathBuf;

use crate::claude::{self, ClaudeContext};
use crate::config::{Config, PaneSettings};
use crate::docker::DockerContext;
use crate::process::{self, ProcessInfo};
#[cfg(target_os = "macos")]
use crate::service::{self, ServiceContext};
use crate::ssh::SshContext;

const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// How often we shell out to `lsof` to (re)discover listening services.
/// Resource sampling for a known service runs on the normal 500ms cadence.
#[cfg(target_os = "macos")]
const SERVICE_DISCOVERY_INTERVAL: Duration = Duration::from_secs(2);

pub struct Pane {
    pub term: Term<VoidListener>,
    processor: Processor,
    writer: Box<dyn Write + Send>,
    rx: mpsc::Receiver<Vec<u8>>,
    master: Box<dyn MasterPty + Send>,
    /// Child shell process. Held so we can SIGKILL it on drop; relying on
    /// SIGHUP propagation alone leaks daemons that were nohup'd or put
    /// themselves in a new session.
    child: Box<dyn Child + Send + Sync>,
    pub cols: u16,
    pub rows: u16,
    pub proc_info: Option<ProcessInfo>,
    last_poll: Instant,
    pub claude: Option<ClaudeContext>,
    pub ssh: Option<SshContext>,
    pub docker: Option<DockerContext>,
    #[cfg(target_os = "macos")]
    pub service: Option<ServiceContext>,
    #[cfg(target_os = "macos")]
    last_service_discovery: Instant,
    /// Accumulates printable chars typed since the last Enter, so we can
    /// surface them as the "last command" for ssh sessions.
    typing_buffer: String,
    pub last_typed: Option<String>,
    /// Cwd captured when the pane spawned — used as the persistence key
    /// for per-pane settings (name, accent color).
    pub initial_cwd: Option<PathBuf>,
    pub settings: PaneSettings,
}

impl Pane {
    pub fn spawn_shell(cols: u16, rows: u16, config: &Config) -> Result<Self> {
        let pty_system = NativePtySystem::default();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        // Use new_default_prog so portable-pty launches $SHELL as a proper
        // login shell (argv[0] = "-zsh"). Required by many shell plugins.
        let mut cmd = CommandBuilder::new_default_prog();
        let initial_cwd = std::env::current_dir().ok();
        if let Some(ref cwd) = initial_cwd {
            cmd.cwd(cwd);
        }
        cmd.env("TERM", "xterm-256color");
        let settings = initial_cwd
            .as_ref()
            .map(|cwd| config.lookup_pane_settings(cwd))
            .unwrap_or_default();
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let writer = pair.master.take_writer()?;

        let size = TermSize::new(cols as usize, rows as usize);
        let term = Term::new(TermConfig::default(), &size, VoidListener);
        let processor = Processor::new();

        Ok(Self {
            term,
            processor,
            writer,
            rx,
            master: pair.master,
            child,
            cols,
            rows,
            proc_info: None,
            last_poll: Instant::now()
                .checked_sub(PROCESS_POLL_INTERVAL)
                .unwrap_or_else(Instant::now),
            claude: None,
            ssh: None,
            docker: None,
            #[cfg(target_os = "macos")]
            service: None,
            #[cfg(target_os = "macos")]
            last_service_discovery: Instant::now()
                .checked_sub(SERVICE_DISCOVERY_INTERVAL)
                .unwrap_or_else(Instant::now),
            typing_buffer: String::new(),
            last_typed: None,
            initial_cwd,
            settings,
        })
    }

    /// Refresh foreground process info (throttled to PROCESS_POLL_INTERVAL).
    /// Also (re)attaches a Claude Code session tailer when the foreground
    /// process is `claude`, and an SSH context when it's `ssh`.
    pub fn poll_process_info(&mut self, config: &Config) {
        if self.last_poll.elapsed() < PROCESS_POLL_INTERVAL {
            return;
        }
        self.last_poll = Instant::now();
        if let Some(pgid) = self.master.process_group_leader() {
            self.proc_info = process::inspect(pgid);
        }
        self.sync_claude();
        self.sync_ssh(config);
        self.sync_docker();
        #[cfg(target_os = "macos")]
        self.sync_service();
    }

    fn sync_docker(&mut self) {
        let info = match &self.proc_info {
            Some(i) if i.is_docker() => i,
            _ => {
                self.docker = None;
                return;
            }
        };
        // Keep existing context if same process (same start_time).
        if let Some(existing) = &self.docker {
            if existing.started_at == info.start_time.unwrap_or(existing.started_at) {
                return;
            }
        }
        self.docker = DockerContext::try_from_proc(info);
    }

    #[cfg(target_os = "macos")]
    fn sync_service(&mut self) {
        // Claude / SSH / Docker panes own the panel when present; skip service detection.
        if self.claude.is_some() || self.ssh.is_some() || self.docker.is_some() {
            self.service = None;
            return;
        }

        let Some(pgid) = self.master.process_group_leader() else {
            self.service = None;
            return;
        };

        // Re-discover listening services only on the slower cadence (lsof is
        // cheap but not free). In between, just refresh resources on the
        // existing service context.
        let should_rediscover = self.last_service_discovery.elapsed() >= SERVICE_DISCOVERY_INTERVAL;

        if should_rediscover {
            self.last_service_discovery = Instant::now();
            match service::detect_service(pgid) {
                Some((pid, cmd_from_lsof, ports)) => {
                    let keep_existing = matches!(&self.service, Some(s) if s.pid == pid);
                    if keep_existing {
                        if let Some(existing) = self.service.as_mut() {
                            existing.ports = ports;
                        }
                    } else {
                        let info = process::inspect(pid);
                        let name = info
                            .as_ref()
                            .map(|i| i.name.clone())
                            .unwrap_or_else(|| cmd_from_lsof.clone());
                        let command = info
                            .as_ref()
                            .filter(|i| !i.argv.is_empty())
                            .map(|i| i.argv.join(" "))
                            .unwrap_or_else(|| cmd_from_lsof.clone());
                        let started_at = info
                            .as_ref()
                            .and_then(|i| i.start_time)
                            .unwrap_or_else(std::time::SystemTime::now);
                        self.service =
                            Some(ServiceContext::new(pid, name, command, ports, started_at));
                    }
                }
                None => {
                    self.service = None;
                }
            }
        }

        if let Some(svc) = self.service.as_mut() {
            svc.sample_resources();
        }
    }

    fn sync_ssh(&mut self, config: &Config) {
        let info = match &self.proc_info {
            Some(i) if i.is_ssh() => i,
            _ => {
                self.ssh = None;
                return;
            }
        };
        // Keep the existing context if it's still for the same ssh invocation
        // (same start_time = same process). Rebuilding would wipe the async
        // DNS result and the started_at.
        if let Some(existing) = &self.ssh {
            if existing.started_at == info.start_time.unwrap_or(existing.started_at) {
                return;
            }
        }
        self.ssh = SshContext::try_from_proc(info, config);
    }

    fn sync_claude(&mut self) {
        let Some(info) = self.proc_info.as_ref().filter(|p| p.is_claude_code()) else {
            self.claude = None;
            return;
        };
        let Some(cwd) = info.cwd.clone() else {
            self.claude = None;
            return;
        };
        let started_at = info.start_time;
        // Keep the cached context only if it's the *same* claude invocation.
        // Matching on cwd alone means a second `claude` run in the same
        // directory keeps tailing the first session's JSONL — which is the
        // stale-data bug this check exists to prevent.
        if let Some(ctx) = &self.claude {
            if ctx.session_cwd == cwd && ctx.session_started_at == started_at {
                return;
            }
        }
        let Some(home) = dirs::home_dir() else { return };
        if let Some(path) = claude::find_session(&home, &cwd, started_at) {
            self.claude = Some(ClaudeContext::new(path, cwd, started_at));
        } else {
            self.claude = None;
        }
    }

    /// Tail the attached Claude session file (if any).
    pub fn tick_claude(&mut self) {
        if let Some(ctx) = &mut self.claude {
            let _ = ctx.tick();
        }
    }

    /// Pull any available PTY output and feed it into the VT emulator.
    pub fn drain(&mut self) {
        while let Ok(chunk) = self.rx.try_recv() {
            self.processor.advance(&mut self.term, &chunk);
        }
    }

    pub fn send_key(&mut self, key: KeyEvent) -> Result<()> {
        self.track_typing(&key);
        let bytes = key_to_bytes(key);
        if !bytes.is_empty() {
            self.writer.write_all(&bytes)?;
            self.writer.flush()?;
        }
        Ok(())
    }

    /// Forward a bracketed-paste payload to the PTY. Wrapping in
    /// `\x1b[200~`…`\x1b[201~` lets the inner program (shell, vim, …)
    /// recognize it as a paste so newlines don't execute mid-stream and
    /// editors don't auto-indent each line.
    pub fn send_paste(&mut self, text: &str) -> Result<()> {
        // Strip the bracketed-paste sentinels if they somehow appear in the
        // payload — otherwise a malicious or sloppy clipboard could end the
        // paste early and inject commands.
        let cleaned = text.replace("\x1b[200~", "").replace("\x1b[201~", "");
        self.writer.write_all(b"\x1b[200~")?;
        self.writer.write_all(cleaned.as_bytes())?;
        self.writer.write_all(b"\x1b[201~")?;
        self.writer.flush()?;
        Ok(())
    }

    /// Update `typing_buffer` / `last_typed` to surface the last line the
    /// user submitted. Best-effort: full-screen TUIs (vim, htop) will pollute
    /// the buffer with navigation keystrokes, but for shell-like sessions
    /// (what SSH usually is) this captures the last command.
    fn track_typing(&mut self, key: &KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => {
                let text = self.typing_buffer.trim();
                if !text.is_empty() {
                    self.last_typed = Some(text.to_string());
                }
                self.typing_buffer.clear();
            }
            KeyCode::Backspace => {
                self.typing_buffer.pop();
            }
            KeyCode::Char('c') | KeyCode::Char('u') | KeyCode::Char('d')
                if ctrl =>
            {
                self.typing_buffer.clear();
            }
            KeyCode::Char(c) if !ctrl => {
                self.typing_buffer.push(c);
            }
            _ => {}
        }
    }

    /// Scroll the viewport. Positive delta = into history (up), negative = back
    /// toward the live edge (down).
    pub fn scroll(&mut self, delta: i32) {
        self.term.scroll_display(Scroll::Delta(delta));
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    /// Extract text between two absolute grid points, inclusive. `line`
    /// follows alacritty's convention: `0..screen_lines` is the live viewport
    /// and negative values address scrollback. Trailing spaces are trimmed
    /// from every line except the last.
    pub fn extract_text(&self, start: (i32, usize), end: (i32, usize)) -> String {
        let (start, end) = order_points(start, end);
        let grid = self.term.grid();
        let rows = grid.screen_lines() as i32;
        let cols = grid.columns();
        if rows == 0 || cols == 0 {
            return String::new();
        }
        let last_col = cols - 1;
        let top_line = -(grid.history_size() as i32);
        let bottom_line = rows - 1;
        let lo = start.0.max(top_line);
        let hi = end.0.min(bottom_line);
        if lo > hi {
            return String::new();
        }

        let mut out = String::new();
        let mut line = lo;
        while line <= hi {
            let col_lo = if line == start.0 { start.1 } else { 0 };
            let col_hi = if line == end.0 { end.1.min(last_col) } else { last_col };
            let mut buf = String::new();
            for col in col_lo..=col_hi {
                let cell = &grid[GridLine(line)][Column(col)];
                let c = cell.c;
                buf.push(if c == '\0' { ' ' } else { c });
            }
            if line == hi {
                out.push_str(&buf);
            } else {
                out.push_str(buf.trim_end_matches(' '));
                out.push('\n');
            }
            line += 1;
        }
        out
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        if cols == self.cols && rows == self.rows {
            return Ok(());
        }
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        self.term.resize(TermSize::new(cols as usize, rows as usize));
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }
}

impl Drop for Pane {
    /// Ensure the shell process is torn down when the pane goes away. SIGHUP
    /// via master-close handles the common case, but a SIGKILL here also
    /// cleans up children that caught HUP (daemons, nohup'd jobs, anything
    /// that put itself in a new session). `try_wait` lets us skip the kill
    /// if the child already exited.
    fn drop(&mut self) {
        if let Ok(Some(_)) = self.child.try_wait() {
            return;
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn order_points(a: (i32, usize), b: (i32, usize)) -> ((i32, usize), (i32, usize)) {
    if a <= b { (a, b) } else { (b, a) }
}

fn key_to_bytes(key: KeyEvent) -> Vec<u8> {
    use KeyCode::*;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let mut out = Vec::new();
    if alt {
        out.push(0x1b);
    }
    match key.code {
        Char(c) => {
            if ctrl && c.is_ascii_alphabetic() {
                let b = (c.to_ascii_lowercase() as u8).wrapping_sub(b'a').wrapping_add(1);
                out.push(b);
            } else if ctrl && c == ' ' {
                out.push(0);
            } else {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
        Enter => out.push(b'\r'),
        Backspace => out.push(0x7f),
        Esc => out.push(0x1b),
        Tab => out.push(b'\t'),
        BackTab => out.extend_from_slice(b"\x1b[Z"),
        Left => out.extend_from_slice(b"\x1b[D"),
        Right => out.extend_from_slice(b"\x1b[C"),
        Up => out.extend_from_slice(b"\x1b[A"),
        Down => out.extend_from_slice(b"\x1b[B"),
        Home => out.extend_from_slice(b"\x1b[H"),
        End => out.extend_from_slice(b"\x1b[F"),
        PageUp => out.extend_from_slice(b"\x1b[5~"),
        PageDown => out.extend_from_slice(b"\x1b[6~"),
        Delete => out.extend_from_slice(b"\x1b[3~"),
        Insert => out.extend_from_slice(b"\x1b[2~"),
        F(n) => {
            let seq: &[u8] = match n {
                1 => b"\x1bOP",
                2 => b"\x1bOQ",
                3 => b"\x1bOR",
                4 => b"\x1bOS",
                5 => b"\x1b[15~",
                6 => b"\x1b[17~",
                7 => b"\x1b[18~",
                8 => b"\x1b[19~",
                9 => b"\x1b[20~",
                10 => b"\x1b[21~",
                11 => b"\x1b[23~",
                12 => b"\x1b[24~",
                _ => b"",
            };
            out.extend_from_slice(seq);
        }
        _ => {}
    }
    out
}
