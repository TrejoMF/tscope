use alacritty_terminal::grid::Dimensions;
use anyhow::Result;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use futures::StreamExt;
use ratatui::prelude::*;
use std::io::Write as _;
use std::time::Duration;
use tokio::time::interval;

use crate::config::{Config, PaneSettings};
use crate::docker;
use crate::pane::Pane;
use crate::theme;
use crate::ui;

pub const CONTEXT_PANEL_ROWS: u16 = 10;

/// Maximum panes rendered side-by-side. Beyond this, extra panes stay alive
/// (draining PTY output) but are hidden until brought into the visible window
/// via the pane picker (`Ctrl-a p`) or focus navigation.
pub const VISIBLE_SLOTS: usize = 3;

pub enum InputMode {
    Normal,
    Prefix,
    /// Typing a name to assign to the focused pane's ssh connection.
    Rename { buffer: String },
    Settings(SettingsState),
    /// Per-pane selection mode. Keyboard-driven cursor + anchor; yanks to the
    /// host terminal's clipboard via OSC 52.
    Copy(CopyState),
    /// Modal list of all panes; pick one to become the primary (leftmost)
    /// of the visible 3-slot window.
    PanePicker(PanePickerState),
    /// Modal listing all Docker containers (`docker ps -a`).
    Docker(DockerState),
    /// Read-only modal listing every keybinding. `scroll` is the row offset
    /// from the top of the content.
    Help { scroll: u16 },
}

#[derive(Debug)]
pub struct PanePickerState {
    pub cursor: usize,
    /// `Some(buffer)` while the user is renaming the pane at `cursor`;
    /// `None` during normal navigation.
    pub editing: Option<String>,
}

#[derive(Debug)]
pub struct DockerState {
    pub containers: Vec<docker::DockerContainer>,
    pub cursor: usize,
}

#[derive(Debug, Clone)]
pub struct CopyState {
    /// Cursor in absolute grid coordinates. `line` follows alacritty's
    /// convention: `0..screen_lines` is the live viewport, negative values
    /// index into scrollback (so `-1` is the row just above the live top).
    pub cursor: (i32, usize),
    /// Selection anchor in the same coordinate space; `None` means no
    /// selection has been started yet.
    pub anchor: Option<(i32, usize)>,
    /// Transient status line message (e.g. "copied N bytes").
    pub notice: Option<String>,
    /// `true` while a mouse-drag selection is in progress. Used so Drag
    /// events know they have an active selection to extend, and so MouseUp
    /// can auto-yank without treating a keyboard-triggered selection as a
    /// drag.
    pub dragging: bool,
}

impl CopyState {
    /// Start the cursor at the bottom of whatever is currently visible, so
    /// entering copy mode while scrolled into history doesn't yank you back.
    pub fn new(rows: u16, display_offset: usize) -> Self {
        let line = rows.saturating_sub(1) as i32 - display_offset as i32;
        Self { cursor: (line, 0), anchor: None, notice: None, dragging: false }
    }

    /// Ordered (start, end) for the current selection, if any.
    pub fn selection_bounds(&self) -> Option<((i32, usize), (i32, usize))> {
        let a = self.anchor?;
        let b = self.cursor;
        Some(if a <= b { (a, b) } else { (b, a) })
    }

    /// Whether a given absolute (line, col) cell falls inside the current
    /// selection. Callers translate viewport rows to absolute lines via
    /// `absolute_line = viewport_row - display_offset`.
    pub fn is_selected(&self, line: i32, col: usize) -> bool {
        let Some((s, e)) = self.selection_bounds() else { return false };
        let p = (line, col);
        p >= s && p <= e
    }
}

#[derive(Debug)]
pub struct SettingsState {
    pub name_buffer: String,
    pub color_idx: usize,
    pub field: SettingsField,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsField {
    Name,
    Color,
}

impl SettingsState {
    pub fn current_color_name(&self) -> &'static str {
        theme::color_name_by_index(self.color_idx)
    }
    pub fn next_color(&mut self) {
        self.color_idx = (self.color_idx + 1) % theme::PALETTE.len();
    }
    pub fn prev_color(&mut self) {
        self.color_idx = if self.color_idx == 0 {
            theme::PALETTE.len() - 1
        } else {
            self.color_idx - 1
        };
    }
    pub fn next_field(&mut self) {
        self.field = match self.field {
            SettingsField::Name => SettingsField::Color,
            SettingsField::Color => SettingsField::Name,
        };
    }
}

pub struct App {
    pub panes: Vec<Pane>,
    pub focus: usize,
    /// Pane index that occupies the leftmost visible slot when the total
    /// pane count exceeds `VISIBLE_SLOTS`. Ignored otherwise.
    pub primary: usize,
    pub quit: bool,
    pub mode: InputMode,
    pub screen: (u16, u16),
    pub config: Config,
}

impl App {
    /// Absolute pane indices currently rendered, in left-to-right order.
    /// When `panes.len() <= VISIBLE_SLOTS` this is simply `0..len`; otherwise
    /// it's a length-`VISIBLE_SLOTS` window starting at `primary`, wrapping.
    pub fn visible_panes(&self) -> Vec<usize> {
        let n = self.panes.len();
        if n == 0 {
            return Vec::new();
        }
        if n <= VISIBLE_SLOTS {
            return (0..n).collect();
        }
        let start = self.primary.min(n - 1);
        (0..VISIBLE_SLOTS).map(|i| (start + i) % n).collect()
    }

    /// Whether there are panes beyond the visible window.
    pub fn has_overflow(&self) -> bool {
        self.panes.len() > VISIBLE_SLOTS
    }

    /// If focus is currently outside the visible window, slide the window so
    /// the focused pane becomes the primary slot. Returns true if `primary`
    /// changed (so the caller can relayout to resize newly-visible panes).
    fn ensure_focus_visible(&mut self) -> bool {
        if !self.has_overflow() {
            return false;
        }
        if self.visible_panes().contains(&self.focus) {
            return false;
        }
        self.primary = self.focus;
        true
    }

    pub fn pane_widths(&self) -> Vec<u16> {
        pane_widths(self.screen.0, self.visible_panes().len().max(1))
    }

    pub fn context_height(&self) -> u16 {
        let pane = self.panes.get(self.focus);
        let has_context = pane
            .map(|p| {
                let base = p.claude.is_some() || p.ssh.is_some() || p.docker.is_some();
                #[cfg(target_os = "macos")]
                let base = base || p.service.is_some();
                base
            })
            .unwrap_or(false);
        if has_context {
            CONTEXT_PANEL_ROWS
        } else {
            0
        }
    }

    pub fn body_rows(&self) -> u16 {
        // screen minus status (1), per-pane header (1), and optional ctx panel
        self.screen
            .1
            .saturating_sub(2 + self.context_height())
            .max(1)
    }

    pub fn relayout(&mut self) -> Result<()> {
        let widths = self.pane_widths();
        let rows = self.body_rows();
        // Only visible panes get resized — hidden panes keep their prior
        // dimensions and will be resized when they slide back into view.
        let visible = self.visible_panes();
        for (slot, pane_idx) in visible.iter().enumerate() {
            let w = widths.get(slot).copied().unwrap_or(1);
            if let Some(pane) = self.panes.get_mut(*pane_idx) {
                pane.resize(w.max(1), rows)?;
            }
        }
        Ok(())
    }

    pub fn add_pane(&mut self) -> Result<()> {
        let pane = Pane::spawn_shell(80, 24, &self.config)?;
        self.panes.push(pane);
        self.focus = self.panes.len() - 1;
        self.ensure_focus_visible();
        self.relayout()?;
        Ok(())
    }

    pub fn close_focused(&mut self) -> Result<()> {
        if self.panes.is_empty() {
            self.quit = true;
            return Ok(());
        }
        self.panes.remove(self.focus);
        if self.panes.is_empty() {
            self.quit = true;
            return Ok(());
        }
        if self.focus >= self.panes.len() {
            self.focus = self.panes.len() - 1;
        }
        if self.primary >= self.panes.len() {
            self.primary = self.panes.len() - 1;
        }
        self.ensure_focus_visible();
        self.relayout()?;
        Ok(())
    }

    /// Remove the pane at `idx` (not necessarily the focused one). Adjusts
    /// `focus` and `primary` so they still point at valid panes; quits if the
    /// last pane is killed.
    pub fn close_pane(&mut self, idx: usize) -> Result<()> {
        if idx >= self.panes.len() {
            return Ok(());
        }
        self.panes.remove(idx);
        if self.panes.is_empty() {
            self.quit = true;
            return Ok(());
        }
        if self.focus > idx {
            self.focus -= 1;
        } else if self.focus >= self.panes.len() {
            self.focus = self.panes.len() - 1;
        }
        if self.primary > idx {
            self.primary -= 1;
        } else if self.primary >= self.panes.len() {
            self.primary = self.panes.len() - 1;
        }
        self.ensure_focus_visible();
        self.relayout()?;
        Ok(())
    }

    pub fn focus_next(&mut self) -> Result<()> {
        if !self.panes.is_empty() {
            self.focus = (self.focus + 1) % self.panes.len();
            if self.ensure_focus_visible() {
                self.relayout()?;
            }
        }
        Ok(())
    }

    pub fn focus_prev(&mut self) -> Result<()> {
        if !self.panes.is_empty() {
            self.focus = if self.focus == 0 {
                self.panes.len() - 1
            } else {
                self.focus - 1
            };
            if self.ensure_focus_visible() {
                self.relayout()?;
            }
        }
        Ok(())
    }

    pub fn focus_n(&mut self, n: usize) -> Result<()> {
        if n < self.panes.len() {
            self.focus = n;
            if self.ensure_focus_visible() {
                self.relayout()?;
            }
        }
        Ok(())
    }

    pub fn open_pane_picker(&mut self) {
        if self.panes.is_empty() {
            return;
        }
        self.mode = InputMode::PanePicker(PanePickerState {
            cursor: self.focus,
            editing: None,
        });
    }

    pub fn open_help(&mut self) {
        self.mode = InputMode::Help { scroll: 0 };
    }

    pub fn open_docker_modal(&mut self) {
        let containers = docker::list_containers();
        self.mode = InputMode::Docker(DockerState {
            containers,
            cursor: 0,
        });
    }

    /// Persist a new name for the pane at `idx`. Empty / whitespace clears it.
    pub fn rename_pane(&mut self, idx: usize, new_name: String) -> Result<()> {
        let Some(pane) = self.panes.get_mut(idx) else { return Ok(()) };
        let trimmed = new_name.trim().to_string();
        let mut ps = pane.settings.clone();
        ps.name = if trimmed.is_empty() { None } else { Some(trimmed) };
        pane.settings = ps.clone();
        if let Some(cwd) = pane.initial_cwd.clone() {
            self.config.set_pane_settings(&cwd, ps);
            self.config.save()?;
        }
        Ok(())
    }

    /// Commit the picker: the chosen pane becomes both the primary of the
    /// visible window and the focused pane.
    pub fn apply_pane_picker(&mut self, cursor: usize) -> Result<()> {
        if cursor >= self.panes.len() {
            return Ok(());
        }
        self.primary = cursor;
        self.focus = cursor;
        self.relayout()?;
        Ok(())
    }

    pub fn open_settings(&mut self) {
        let Some(pane) = self.panes.get(self.focus) else { return };
        let current_name = pane.settings.name.clone().unwrap_or_default();
        let current_color = pane
            .settings
            .color
            .as_deref()
            .map(theme::color_index)
            .unwrap_or(0);
        self.mode = InputMode::Settings(SettingsState {
            name_buffer: current_name,
            color_idx: current_color,
            field: SettingsField::Name,
        });
    }

    /// Commit the settings modal state to the focused pane and persist.
    pub fn apply_settings(&mut self, state: SettingsState) -> Result<()> {
        let Some(pane) = self.panes.get_mut(self.focus) else { return Ok(()) };
        let name = state.name_buffer.trim().to_string();
        let color_name = state.current_color_name();

        let mut ps = PaneSettings::default();
        if !name.is_empty() {
            ps.name = Some(name.clone());
        }
        // Treat "blue" (the default) as "unset" so config stays tidy.
        if color_name != "blue" {
            ps.color = Some(color_name.to_string());
        }
        pane.settings = ps.clone();

        if let Some(cwd) = pane.initial_cwd.clone() {
            self.config.set_pane_settings(&cwd, ps);
            self.config.save()?;
        }
        Ok(())
    }

    pub fn enter_copy_mode(&mut self) {
        let Some(pane) = self.panes.get(self.focus) else { return };
        let display_offset = pane.term.grid().display_offset();
        self.mode = InputMode::Copy(CopyState::new(pane.rows, display_offset));
    }

    /// Exit copy mode and snap the focused pane back to the live edge.
    pub fn exit_copy_mode(&mut self) {
        self.mode = InputMode::Normal;
        if let Some(pane) = self.panes.get_mut(self.focus) {
            pane.scroll_to_bottom();
        }
    }

    /// Which pane covers screen column `x`, if any. Returns an absolute pane
    /// index (into `self.panes`) for whichever visible slot contains `x`.
    pub fn pane_at_x(&self, x: u16) -> Option<usize> {
        let widths = self.pane_widths();
        let visible = self.visible_panes();
        let last = widths.len().saturating_sub(1);
        let mut acc: u16 = 0;
        for (slot, w) in widths.iter().enumerate() {
            // Include the divider column after this pane (if any) so a click
            // directly on the divider maps to the pane on its left.
            let step = *w + if slot < last { 1 } else { 0 };
            acc = acc.saturating_add(step);
            if x < acc {
                return visible.get(slot).copied();
            }
        }
        None
    }

    /// Resolve a screen (x, y) to a pane and the cell inside it. Returns
    /// `(pane_idx, abs_line, col)` where `abs_line` is in the same absolute
    /// coordinate space as `CopyState.cursor` (negative = scrollback).
    ///
    /// Returns `None` if the click landed on the pane header, below the body,
    /// or outside any pane horizontally.
    pub fn pane_cell_at(&self, x: u16, y: u16) -> Option<(usize, i32, usize)> {
        let idx = self.pane_at_x(x)?;
        if y == 0 {
            return None;
        }
        let pane = self.panes.get(idx)?;
        let body_row = (y - 1) as usize;
        if body_row >= pane.rows as usize {
            return None;
        }
        let widths = self.pane_widths();
        let visible = self.visible_panes();
        let slot = visible.iter().position(|i| *i == idx)?;
        let x_start: u16 = widths.iter().take(slot).sum::<u16>() + slot as u16;
        let col = x.saturating_sub(x_start) as usize;
        let col = col.min((pane.cols as usize).saturating_sub(1));
        let display_offset = pane.term.grid().display_offset() as i32;
        let abs_line = body_row as i32 - display_offset;
        Some((idx, abs_line, col))
    }

    /// Clamp a screen (x, y) to the interior of the given pane and return
    /// `(abs_line, col)`. Used while dragging so the selection cursor stays
    /// inside the pane the drag started in, even if the mouse wanders into
    /// a neighbour or beyond the pane's body.
    pub fn pane_cell_clamped(&self, pane_idx: usize, x: u16, y: u16) -> Option<(i32, usize)> {
        let pane = self.panes.get(pane_idx)?;
        let widths = self.pane_widths();
        let visible = self.visible_panes();
        let slot = visible.iter().position(|i| *i == pane_idx)?;
        let x_start: u16 = widths.iter().take(slot).sum::<u16>() + slot as u16;
        let x_end = x_start + widths[slot];
        let col = if x < x_start {
            0
        } else if x >= x_end {
            (pane.cols as usize).saturating_sub(1)
        } else {
            (x - x_start) as usize
        };
        let col = col.min((pane.cols as usize).saturating_sub(1));
        let body_row = if y <= 0 {
            0
        } else {
            let r = (y - 1) as usize;
            r.min((pane.rows as usize).saturating_sub(1))
        };
        let display_offset = pane.term.grid().display_offset() as i32;
        Some((body_row as i32 - display_offset, col))
    }

    /// Begin a rename prompt if the focused pane is an ssh connection.
    /// Pre-populates the buffer with the existing name (if any).
    pub fn start_rename(&mut self) {
        let Some(pane) = self.panes.get(self.focus) else { return };
        if pane.ssh.is_none() {
            return;
        }
        let initial = pane
            .ssh
            .as_ref()
            .and_then(|s| s.display_name.clone())
            .unwrap_or_default();
        self.mode = InputMode::Rename { buffer: initial };
    }

    /// Commit the current rename buffer to the focused pane's ssh context
    /// and persist it to the config file. An empty name removes the alias.
    pub fn apply_rename(&mut self, buffer: String) -> Result<()> {
        let name = buffer.trim().to_string();
        let Some(pane) = self.panes.get_mut(self.focus) else { return Ok(()) };
        let Some(ssh) = pane.ssh.as_mut() else { return Ok(()) };
        let (user, host) = (ssh.user.clone(), ssh.host.clone());

        if name.is_empty() {
            ssh.display_name = None;
        } else {
            ssh.display_name = Some(name.clone());
        }
        self.config.set_ssh_alias(user.as_deref(), &host, name);
        self.config.save()?;
        Ok(())
    }
}

/// Split `screen_w` across `n` panes, reserving one column between each
/// adjacent pair for a visual divider. Returns the width of each pane (the
/// dividers are not part of any pane's rect).
pub fn pane_widths(screen_w: u16, n: usize) -> Vec<u16> {
    let n_u16 = n.max(1) as u16;
    let dividers = n_u16.saturating_sub(1);
    let usable = screen_w.saturating_sub(dividers);
    let base = usable / n_u16;
    let rem = usable % n_u16;
    (0..n)
        .map(|i| base + if (i as u16) < rem { 1 } else { 0 })
        .collect()
}

pub async fn run<B: Backend>(terminal: &mut Terminal<B>) -> Result<()> {
    let size = terminal.size()?;
    let mut app = App {
        panes: Vec::new(),
        focus: 0,
        primary: 0,
        quit: false,
        mode: InputMode::Normal,
        screen: (size.width, size.height),
        config: Config::load(),
    };
    app.add_pane()?;

    let mut events = EventStream::new();
    let mut tick = interval(Duration::from_millis(16));

    while !app.quit {
        let ctx_before = app.context_height();
        // Snapshot config so the mutable borrows of panes and the shared-ref
        // borrow of config don't collide.
        let cfg_snapshot = std::mem::take(&mut app.config);
        for pane in &mut app.panes {
            pane.drain();
            pane.poll_process_info(&cfg_snapshot);
            pane.tick_claude();
        }
        app.config = cfg_snapshot;
        if app.context_height() != ctx_before {
            app.relayout()?;
        }
        terminal.draw(|f| ui::draw(f, &app))?;

        tokio::select! {
            _ = tick.tick() => {}
            maybe_ev = events.next() => {
                if let Some(Ok(ev)) = maybe_ev {
                    let ctx_before_ev = app.context_height();
                    handle_event(&mut app, ev)?;
                    if app.context_height() != ctx_before_ev {
                        app.relayout()?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn handle_event(app: &mut App, ev: Event) -> Result<()> {
    match ev {
        Event::Key(key) => handle_key(app, key)?,
        Event::Mouse(me) => handle_mouse(app, me)?,
        Event::Paste(text) => handle_paste(app, text)?,
        Event::Resize(w, h) => {
            app.screen = (w, h);
            app.relayout()?;
        }
        _ => {}
    }
    Ok(())
}

fn handle_paste(app: &mut App, text: String) -> Result<()> {
    match &mut app.mode {
        InputMode::Normal => {
            if let Some(pane) = app.panes.get_mut(app.focus) {
                pane.send_paste(&text)?;
            }
        }
        InputMode::Rename { buffer } => {
            // Single-line prompt: collapse newlines to spaces.
            for ch in text.chars() {
                if ch == '\n' || ch == '\r' {
                    buffer.push(' ');
                } else if !ch.is_control() {
                    buffer.push(ch);
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn handle_mouse(app: &mut App, me: MouseEvent) -> Result<()> {
    match me.kind {
        MouseEventKind::ScrollUp => {
            if let Some(idx) = app.pane_at_x(me.column) {
                if let Some(p) = app.panes.get_mut(idx) {
                    p.scroll(3);
                }
            }
        }
        MouseEventKind::ScrollDown => {
            if let Some(idx) = app.pane_at_x(me.column) {
                if let Some(p) = app.panes.get_mut(idx) {
                    p.scroll(-3);
                }
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let Some((pane_idx, abs_line, col)) = app.pane_cell_at(me.column, me.row) else {
                return Ok(());
            };
            // Left-click anchors a new selection in the clicked pane. Switch
            // focus so the existing copy-mode plumbing (rendering, key
            // handling, extract_text) all operate on the right pane.
            app.focus = pane_idx;
            let (rows, display_offset) = {
                let pane = &app.panes[pane_idx];
                (pane.rows, pane.term.grid().display_offset())
            };
            let mut state = CopyState::new(rows, display_offset);
            state.cursor = (abs_line, col);
            state.anchor = Some((abs_line, col));
            state.dragging = true;
            app.mode = InputMode::Copy(state);
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let dragging = matches!(&app.mode, InputMode::Copy(s) if s.dragging);
            if !dragging {
                return Ok(());
            }
            let focus = app.focus;
            // Auto-scroll when the pointer leaves the pane body vertically —
            // lets a single drag span more than one screenful.
            let pane_rows = app.panes.get(focus).map(|p| p.rows).unwrap_or(0);
            if me.row == 0 {
                if let Some(p) = app.panes.get_mut(focus) { p.scroll(1); }
            } else if me.row >= 1 + pane_rows {
                if let Some(p) = app.panes.get_mut(focus) { p.scroll(-1); }
            }
            let Some((abs_line, col)) = app.pane_cell_clamped(focus, me.column, me.row) else {
                return Ok(());
            };
            if let InputMode::Copy(state) = &mut app.mode {
                state.cursor = (abs_line, col);
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let InputMode::Copy(state) = &app.mode else { return Ok(()) };
            if !state.dragging {
                return Ok(());
            }
            let bounds = state.selection_bounds();
            let empty = match bounds {
                Some((a, b)) => a == b,
                None => true,
            };
            if empty {
                // Single click without drag: leave copy mode so the click
                // acts like a focus tap.
                app.exit_copy_mode();
                return Ok(());
            }
            let (start, end) = bounds.unwrap();
            let text = app.panes[app.focus].extract_text(start, end);
            let bytes = text.len();
            if bytes > 0 {
                copy_to_clipboard(&text)?;
            }
            if let InputMode::Copy(state) = &mut app.mode {
                state.dragging = false;
                state.notice = Some(format!("copied {} bytes", bytes));
            }
        }
        _ => {}
    }
    Ok(())
}

fn handle_key(app: &mut App, key: KeyEvent) -> Result<()> {
    // Settings modal.
    if matches!(app.mode, InputMode::Settings(_)) {
        handle_settings_key(app, key)?;
        return Ok(());
    }

    // Pane picker modal.
    if matches!(app.mode, InputMode::PanePicker(_)) {
        handle_pane_picker_key(app, key)?;
        return Ok(());
    }

    // Docker modal.
    if matches!(app.mode, InputMode::Docker(_)) {
        handle_docker_key(app, key)?;
        return Ok(());
    }

    // Help modal: read-only, scrollable, closes on Esc/q.
    if matches!(app.mode, InputMode::Help { .. }) {
        handle_help_key(app, key);
        return Ok(());
    }

    // Copy mode: keyboard-driven selection and yank.
    if matches!(app.mode, InputMode::Copy(_)) {
        handle_copy_key(app, key)?;
        return Ok(());
    }

    // Rename mode: keystrokes edit the alias buffer, Enter saves, Esc cancels.
    if matches!(app.mode, InputMode::Rename { .. }) {
        match key.code {
            KeyCode::Esc => {
                app.mode = InputMode::Normal;
            }
            KeyCode::Enter => {
                let buffer = match std::mem::replace(&mut app.mode, InputMode::Normal) {
                    InputMode::Rename { buffer } => buffer,
                    _ => String::new(),
                };
                app.apply_rename(buffer)?;
            }
            KeyCode::Backspace => {
                if let InputMode::Rename { buffer } = &mut app.mode {
                    buffer.pop();
                }
            }
            KeyCode::Char(c) => {
                if let InputMode::Rename { buffer } = &mut app.mode {
                    buffer.push(c);
                }
            }
            _ => {}
        }
        return Ok(());
    }

    // Prefix mode: interpret next keystroke as a tscope command.
    if matches!(app.mode, InputMode::Prefix) {
        app.mode = InputMode::Normal;
        match key.code {
            KeyCode::Char('n') => app.add_pane()?,
            KeyCode::Char('x') => app.close_focused()?,
            KeyCode::Char('h') | KeyCode::Left => app.focus_prev()?,
            KeyCode::Char('l') | KeyCode::Right => app.focus_next()?,
            KeyCode::Char('q') => app.quit = true,
            KeyCode::Char('r') => app.start_rename(),
            KeyCode::Char('s') => app.open_settings(),
            KeyCode::Char('p') => app.open_pane_picker(),
            KeyCode::Char('d') => app.open_docker_modal(),
            KeyCode::Char('i') => app.open_help(),
            KeyCode::Char('[') => app.enter_copy_mode(),
            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                let n = (c as u8 - b'1') as usize;
                app.focus_n(n)?;
            }
            // Ctrl-a a: pass a literal Ctrl-a through to the pane
            KeyCode::Char('a') => {
                if let Some(pane) = app.panes.get_mut(app.focus) {
                    let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
                    pane.send_key(ctrl_a)?;
                }
            }
            _ => {}
        }
        return Ok(());
    }

    // Ctrl-a enters prefix mode.
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('a')) {
        app.mode = InputMode::Prefix;
        return Ok(());
    }

    // Ctrl-q is a direct emergency quit.
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q'))
    {
        app.quit = true;
        return Ok(());
    }

    if let Some(pane) = app.panes.get_mut(app.focus) {
        pane.send_key(key)?;
    }
    Ok(())
}

fn handle_copy_key(app: &mut App, key: KeyEvent) -> Result<()> {
    // Snapshot the pane geometry up front so we can clamp against scrollback
    // bounds without holding borrows across the mode match.
    let (screen_lines, history_size, max_col) = match app.panes.get(app.focus) {
        Some(p) => {
            let g = p.term.grid();
            (
                g.screen_lines() as i32,
                g.history_size() as i32,
                (p.cols.saturating_sub(1)) as usize,
            )
        }
        None => {
            app.mode = InputMode::Normal;
            return Ok(());
        }
    };
    let top_line = -history_size;
    let bottom_line = screen_lines - 1;

    enum Action {
        None,
        Cancel,
        Yank,
        /// New cursor line after clamping. The caller reconciles the pane's
        /// display_offset so the cursor stays visible.
        FollowCursor,
    }

    let action = {
        let InputMode::Copy(state) = &mut app.mode else {
            return Ok(());
        };
        state.notice = None;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => Action::Cancel,
            KeyCode::Char('h') | KeyCode::Left => {
                if state.cursor.1 > 0 {
                    state.cursor.1 -= 1;
                }
                Action::None
            }
            KeyCode::Char('l') | KeyCode::Right => {
                if state.cursor.1 < max_col {
                    state.cursor.1 += 1;
                }
                Action::None
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if state.cursor.0 < bottom_line {
                    state.cursor.0 += 1;
                }
                Action::FollowCursor
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if state.cursor.0 > top_line {
                    state.cursor.0 -= 1;
                }
                Action::FollowCursor
            }
            KeyCode::Char('0') | KeyCode::Home => {
                state.cursor.1 = 0;
                Action::None
            }
            KeyCode::Char('$') | KeyCode::End => {
                state.cursor.1 = max_col;
                Action::None
            }
            KeyCode::Char('g') => {
                state.cursor.0 = top_line;
                Action::FollowCursor
            }
            KeyCode::Char('G') => {
                state.cursor.0 = bottom_line;
                Action::FollowCursor
            }
            KeyCode::Char('v') | KeyCode::Char(' ') => {
                state.anchor = match state.anchor {
                    Some(_) => None,
                    None => Some(state.cursor),
                };
                Action::None
            }
            KeyCode::Char('y') | KeyCode::Enter => Action::Yank,
            _ => Action::None,
        }
    };

    match action {
        Action::None => {}
        Action::Cancel => app.exit_copy_mode(),
        Action::FollowCursor => {
            let InputMode::Copy(state) = &app.mode else { return Ok(()) };
            let cursor_line = state.cursor.0;
            let Some(pane) = app.panes.get_mut(app.focus) else { return Ok(()) };
            // Keep the cursor visible: viewport shows absolute lines
            // [-display_offset .. -display_offset + screen_lines). Adjust
            // display_offset so cursor_line falls inside that window.
            let display_offset = pane.term.grid().display_offset() as i32;
            let view_top = -display_offset;
            let view_bot = view_top + screen_lines - 1;
            if cursor_line < view_top {
                pane.scroll(view_top - cursor_line);
            } else if cursor_line > view_bot {
                pane.scroll(-(cursor_line - view_bot));
            }
        }
        Action::Yank => {
            let bounds = {
                let InputMode::Copy(state) = &app.mode else { return Ok(()) };
                state.selection_bounds().unwrap_or((state.cursor, state.cursor))
            };
            let Some(pane) = app.panes.get(app.focus) else { return Ok(()) };
            let text = pane.extract_text(bounds.0, bounds.1);
            let bytes = text.len();
            copy_to_clipboard(&text)?;
            if bytes == 0 {
                app.exit_copy_mode();
            } else if let InputMode::Copy(state) = &mut app.mode {
                state.notice = Some(format!("copied {} bytes", bytes));
                state.anchor = None;
            }
        }
    }
    Ok(())
}

/// Put `text` on the system clipboard. Tries the native clipboard API first
/// (reliable on desktop) and also emits OSC 52 so the copy still works when
/// the app is running inside an SSH session on a remote host.
fn copy_to_clipboard(text: &str) -> Result<()> {
    if let Ok(mut clip) = arboard::Clipboard::new() {
        let _ = clip.set_text(text.to_string());
    }
    let encoded = base64_encode(text.as_bytes());
    let mut stdout = std::io::stdout().lock();
    write!(stdout, "\x1b]52;c;{}\x07", encoded)?;
    stdout.flush()?;
    Ok(())
}

fn base64_encode(input: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(T[(b0 >> 2) as usize] as char);
        out.push(T[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(T[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(T[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn handle_pane_picker_key(app: &mut App, key: KeyEvent) -> Result<()> {
    let n = app.panes.len();

    // Rename sub-mode: consume text input, don't let it fall through to
    // navigation / quick-select shortcuts.
    let is_editing = matches!(
        &app.mode,
        InputMode::PanePicker(s) if s.editing.is_some()
    );
    if is_editing {
        match key.code {
            KeyCode::Esc => {
                if let InputMode::PanePicker(s) = &mut app.mode {
                    s.editing = None;
                }
            }
            KeyCode::Enter => {
                let (cursor, buffer) = match &mut app.mode {
                    InputMode::PanePicker(s) => {
                        let buf = s.editing.take().unwrap_or_default();
                        (s.cursor, buf)
                    }
                    _ => return Ok(()),
                };
                app.rename_pane(cursor, buffer)?;
            }
            KeyCode::Backspace => {
                if let InputMode::PanePicker(s) = &mut app.mode {
                    if let Some(buf) = s.editing.as_mut() {
                        buf.pop();
                    }
                }
            }
            KeyCode::Char(c) => {
                if let InputMode::PanePicker(s) = &mut app.mode {
                    if let Some(buf) = s.editing.as_mut() {
                        buf.push(c);
                    }
                }
            }
            _ => {}
        }
        return Ok(());
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.mode = InputMode::Normal;
        }
        KeyCode::Enter => {
            let cursor = match std::mem::replace(&mut app.mode, InputMode::Normal) {
                InputMode::PanePicker(s) => s.cursor,
                _ => return Ok(()),
            };
            app.apply_pane_picker(cursor)?;
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if let InputMode::PanePicker(s) = &mut app.mode {
                if n > 0 {
                    s.cursor = (s.cursor + 1) % n;
                }
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if let InputMode::PanePicker(s) = &mut app.mode {
                if n > 0 {
                    s.cursor = if s.cursor == 0 { n - 1 } else { s.cursor - 1 };
                }
            }
        }
        // Kill the pane under the cursor. Uppercase-only so lowercase `k`
        // still means "cursor up" and accidental kills are harder.
        KeyCode::Char('K') => {
            let cursor = match &app.mode {
                InputMode::PanePicker(s) => s.cursor,
                _ => return Ok(()),
            };
            app.close_pane(cursor)?;
            if app.panes.is_empty() {
                app.mode = InputMode::Normal;
                return Ok(());
            }
            if let InputMode::PanePicker(s) = &mut app.mode {
                if s.cursor >= app.panes.len() {
                    s.cursor = app.panes.len() - 1;
                }
            }
        }
        // Spawn a new shell pane and park the cursor on it, without leaving
        // the picker — user can then rename (R) or commit (Enter).
        KeyCode::Char('n') | KeyCode::Char('N') => {
            app.add_pane()?;
            let new_idx = app.panes.len().saturating_sub(1);
            if let InputMode::PanePicker(s) = &mut app.mode {
                s.cursor = new_idx;
            }
        }
        // Enter rename mode for the pane under the cursor, prefilled with its
        // current name.
        KeyCode::Char('r') | KeyCode::Char('R') => {
            let cursor = match &app.mode {
                InputMode::PanePicker(s) => s.cursor,
                _ => return Ok(()),
            };
            let prefill = app
                .panes
                .get(cursor)
                .and_then(|p| p.settings.name.clone())
                .unwrap_or_default();
            if let InputMode::PanePicker(s) = &mut app.mode {
                s.editing = Some(prefill);
            }
        }
        // Quick-select pane 1-9 by number, committing immediately.
        KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
            let idx = (c as u8 - b'1') as usize;
            if idx < n {
                app.mode = InputMode::Normal;
                app.apply_pane_picker(idx)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn handle_help_key(app: &mut App, key: KeyEvent) {
    // Visible rows = the modal's inner height (screen height minus borders +
    // padding, minus the status line at the bottom). Mirrors the sizing
    // logic in `render_help_modal`.
    let content_rows = crate::ui::help_content_rows();
    let max_modal_height = app.screen.1.saturating_sub(2);
    let modal_height = (content_rows + 4).min(max_modal_height).max(6);
    let visible_rows = modal_height.saturating_sub(4);
    let max_scroll = content_rows.saturating_sub(visible_rows);

    let InputMode::Help { scroll } = &mut app.mode else {
        return;
    };
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
            app.mode = InputMode::Normal;
        }
        KeyCode::Char('j') | KeyCode::Down => {
            *scroll = scroll.saturating_add(1).min(max_scroll);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            *scroll = scroll.saturating_sub(1);
        }
        KeyCode::PageDown => {
            *scroll = scroll.saturating_add(visible_rows.max(1)).min(max_scroll);
        }
        KeyCode::PageUp => {
            *scroll = scroll.saturating_sub(visible_rows.max(1));
        }
        KeyCode::Home | KeyCode::Char('g') => {
            *scroll = 0;
        }
        KeyCode::End | KeyCode::Char('G') => {
            *scroll = max_scroll;
        }
        _ => {}
    }
}

fn handle_docker_key(app: &mut App, key: KeyEvent) -> Result<()> {
    let n = match &app.mode {
        InputMode::Docker(s) => s.containers.len(),
        _ => return Ok(()),
    };
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.mode = InputMode::Normal;
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if let InputMode::Docker(s) = &mut app.mode {
                if n > 0 {
                    s.cursor = (s.cursor + 1) % n;
                }
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if let InputMode::Docker(s) = &mut app.mode {
                if n > 0 {
                    s.cursor = if s.cursor == 0 { n - 1 } else { s.cursor - 1 };
                }
            }
        }
        // Refresh the container list.
        KeyCode::Char('r') | KeyCode::Char('R') => {
            if let InputMode::Docker(s) = &mut app.mode {
                s.containers = docker::list_containers();
                if s.cursor >= s.containers.len() {
                    s.cursor = s.containers.len().saturating_sub(1);
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn handle_settings_key(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Esc => {
            app.mode = InputMode::Normal;
        }
        KeyCode::Enter => {
            if let InputMode::Settings(state) =
                std::mem::replace(&mut app.mode, InputMode::Normal)
            {
                app.apply_settings(state)?;
            }
        }
        KeyCode::Tab | KeyCode::BackTab => {
            if let InputMode::Settings(state) = &mut app.mode {
                state.next_field();
            }
        }
        KeyCode::Left => {
            if let InputMode::Settings(state) = &mut app.mode {
                if state.field == SettingsField::Color {
                    state.prev_color();
                }
            }
        }
        KeyCode::Right => {
            if let InputMode::Settings(state) = &mut app.mode {
                if state.field == SettingsField::Color {
                    state.next_color();
                }
            }
        }
        KeyCode::Backspace => {
            if let InputMode::Settings(state) = &mut app.mode {
                if state.field == SettingsField::Name {
                    state.name_buffer.pop();
                }
            }
        }
        KeyCode::Char(c) => {
            if let InputMode::Settings(state) = &mut app.mode {
                match state.field {
                    SettingsField::Name => state.name_buffer.push(c),
                    SettingsField::Color => match c {
                        'h' => state.prev_color(),
                        'l' => state.next_color(),
                        _ => {}
                    },
                }
            }
        }
        _ => {}
    }
    Ok(())
}
