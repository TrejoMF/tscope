use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line as GridLine};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color as AlacColor, NamedColor};
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap};

use crate::app::{App, CopyState, InputMode, PanePickerState, SettingsField, SettingsState};
use crate::pane::Pane;
use crate::theme;

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let ctx_rows = app.context_height();

    let (pane_area, ctx_area, status_area) = if ctx_rows > 0 {
        let chunks = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(ctx_rows),
            Constraint::Length(1),
        ])
        .split(area);
        (chunks[0], Some(chunks[1]), chunks[2])
    } else {
        let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
        (chunks[0], None, chunks[1])
    };

    // Horizontal split: one column per *visible* pane. Panes beyond the
    // visible window stay alive (PTY keeps draining) but aren't drawn or
    // resized — they'll be resized the next time they slide into view.
    let widths = app.pane_widths();
    let visible = app.visible_panes();
    if !widths.is_empty() && !visible.is_empty() {
        let constraints: Vec<Constraint> =
            widths.iter().map(|w| Constraint::Length(*w)).collect();
        let pane_rects = Layout::horizontal(constraints).split(pane_area);
        for (slot, &pane_idx) in visible.iter().enumerate() {
            let Some(pane) = app.panes.get(pane_idx) else { continue };
            let rect = pane_rects[slot];
            let copy = if pane_idx == app.focus {
                if let InputMode::Copy(state) = &app.mode { Some(state) } else { None }
            } else {
                None
            };
            render_pane(f, pane, rect, pane_idx, pane_idx == app.focus, copy);
        }
    }

    if let Some(ctx_area) = ctx_area {
        render_context_panel(f, app, ctx_area);
    }

    render_status_bar(f, app, status_area);

    // Modal overlay drawn last so it sits above everything.
    if let InputMode::Settings(state) = &app.mode {
        render_settings_modal(f, app, state, area);
    }
    if let InputMode::PanePicker(state) = &app.mode {
        render_pane_picker_modal(f, app, state, area);
    }
}

fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
    // While renaming, the status bar becomes the input prompt.
    if let InputMode::Rename { buffer } = &app.mode {
        let prompt = Line::from(vec![
            Span::styled(
                " RENAME ",
                Style::default()
                    .bg(Color::Yellow)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                buffer.clone(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("▍", Style::default().fg(Color::Yellow)),
            Span::raw("   "),
            Span::styled(
                "Enter to save · Esc to cancel",
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        let p = Paragraph::new(prompt).style(Style::default().bg(Color::Black));
        f.render_widget(p, area);
        return;
    }

    let mode_label = match app.mode {
        InputMode::Normal => "NORMAL",
        InputMode::Prefix => "PREFIX",
        InputMode::Rename { .. } => "RENAME",
        InputMode::Settings(_) => "SETTINGS",
        InputMode::Copy(_) => "COPY",
        InputMode::PanePicker(_) => "PANES",
    };
    let help_owned = match &app.mode {
        InputMode::Prefix => {
            "n=new x=close h/l=switch 1-9=jump p=panes r=rename-ssh s=settings [=copy q=quit a=literal"
                .to_string()
        }
        InputMode::Copy(state) => {
            if let Some(msg) = &state.notice {
                format!("{}  ·  hjkl=move v=select y=yank Esc=quit", msg)
            } else {
                "hjkl/arrows=move 0/$=line g/G=top/bot v=select y/Enter=yank Esc=exit".to_string()
            }
        }
        InputMode::PanePicker(_) => {
            "↑/↓=move 1-9=quick-select Enter=show Esc=cancel".to_string()
        }
        _ => "Ctrl-a: n=new x=close h/l=switch 1-9=jump p=panes r=rename-ssh s=settings [=copy q=quit"
            .to_string(),
    };
    let style = if matches!(app.mode, InputMode::Copy(_)) {
        Style::default().bg(Color::Rgb(0x44, 0x3a, 0x78)).fg(Color::White)
    } else {
        Style::default().bg(Color::DarkGray).fg(Color::White)
    };

    // Compose the status as styled spans so the overflow indicator can stand
    // out visually when the user has more panes than visible slots.
    let total = app.panes.len();
    let visible_count = app.visible_panes().len();
    let hidden = total.saturating_sub(visible_count);

    let mut spans: Vec<Span<'static>> = vec![
        Span::raw(" tscope  |  "),
        Span::raw(format!("panes: {}/{}", visible_count, total)),
    ];
    if hidden > 0 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!(" ⋯ +{} hidden (Ctrl-a p) ", hidden),
            Style::default()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::raw(format!(
        "  |  focus: {}  |  {}  |  {} ",
        app.focus + 1,
        mode_label,
        help_owned,
    )));

    let status = Paragraph::new(Line::from(spans)).style(style);
    f.render_widget(status, area);
}

fn render_pane(
    f: &mut Frame,
    pane: &Pane,
    rect: Rect,
    idx: usize,
    focused: bool,
    copy: Option<&CopyState>,
) {
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(rect);
    let header_area = chunks[0];
    let body_area = chunks[1];

    let accent = pane
        .settings
        .color
        .as_deref()
        .and_then(theme::color_from_name)
        .unwrap_or(Color::Blue);
    let header_style = if focused {
        Style::default()
            .bg(accent)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().bg(Color::DarkGray).fg(Color::Gray)
    };
    let label = pane_header_label(pane, idx, focused, header_area.width as usize);
    let header = Paragraph::new(label).style(header_style);
    f.render_widget(header, header_area);

    let grid = pane.term.grid();
    let total_rows = grid.screen_lines();
    let total_cols = grid.columns();
    let render_rows = (body_area.height as usize).min(total_rows);
    let render_cols = (body_area.width as usize).min(total_cols);
    // Viewport row r maps to absolute grid line `r - display_offset`. Without
    // this shift, `grid[Line(r)]` always returns the live viewport regardless
    // of scroll position.
    let display_offset = grid.display_offset() as i32;

    let selection_style = Style::default().bg(Color::Rgb(0x44, 0x3a, 0x78)).fg(Color::White);
    let cursor_style = Style::default().bg(Color::Yellow).fg(Color::Black);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(render_rows);
    for row in 0..render_rows {
        let abs_line = row as i32 - display_offset;
        let line_idx = GridLine(abs_line);
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut buf = String::new();
        let mut current_style = Style::default();
        for col in 0..render_cols {
            let cell = &grid[line_idx][Column(col)];
            let mut style = cell_to_style(cell.fg, cell.bg, cell.flags);
            if let Some(cs) = copy {
                if cs.is_selected(abs_line, col) {
                    style = selection_style;
                }
                if cs.cursor == (abs_line, col) {
                    style = cursor_style;
                }
            }
            if style != current_style && !buf.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut buf), current_style));
            }
            current_style = style;
            let c = cell.c;
            buf.push(if c == '\0' { ' ' } else { c });
        }
        if !buf.is_empty() {
            spans.push(Span::styled(buf, current_style));
        }
        lines.push(Line::from(spans));
    }

    f.render_widget(Paragraph::new(lines), body_area);
}

fn render_context_panel(f: &mut Frame, app: &App, area: Rect) {
    let Some(pane) = app.panes.get(app.focus) else {
        return;
    };
    if let Some(ctx) = pane.claude.as_ref() {
        render_claude_panel(f, ctx, area);
        return;
    }
    if let Some(ssh) = pane.ssh.as_ref() {
        render_ssh_panel(f, pane, ssh, area);
        return;
    }
    #[cfg(target_os = "macos")]
    if let Some(svc) = pane.service.as_ref() {
        render_service_panel(f, svc, area);
    }
}

fn render_claude_panel(f: &mut Frame, ctx: &crate::claude::ClaudeContext, area: Rect) {

    let session_display = shorten_home(&ctx.session_path);
    let branch = ctx.git_branch.as_deref().unwrap_or("?");
    let tool_total = ctx.tool_count_total();
    let top_tools = ctx.top_tools(3);

    // --- titled bordered block ----------------------------------------------
    let title_line = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            "⚡ claude",
            Style::default()
                .fg(Color::LightMagenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", Style::default()),
        dim_sep(),
        Span::styled(
            format!(" {} ", session_display),
            Style::default().fg(Color::White),
        ),
        dim_sep(),
        Span::raw(" "),
        Span::styled("⎇ ", Style::default().fg(Color::Green)),
        Span::styled(
            branch,
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        dim_sep(),
        Span::raw(" "),
        Span::styled(
            format!("turn {}", ctx.turn_count),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw(" "),
        dim_sep(),
        Span::raw(" "),
        Span::styled(
            format!("⚒ {}", tool_total),
            Style::default().fg(Color::Yellow),
        ),
        if top_tools.is_empty() {
            Span::raw("")
        } else {
            Span::styled(
                format!(" ({})", top_tools),
                Style::default().fg(Color::DarkGray),
            )
        },
        Span::raw(" "),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Magenta))
        .title(title_line)
        .title_alignment(Alignment::Left)
        .padding(Padding::horizontal(1));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // --- inner layout --------------------------------------------------------
    // Rows: [tool=1][about wrap=2][trail wrap=2][you=1] = 6 inside an 8-row inner.
    let chunks = Layout::vertical([
        Constraint::Length(1), // tool
        Constraint::Length(2), // about (wraps)
        Constraint::Min(2),    // trail (wraps, grows)
        Constraint::Length(1), // you
    ])
    .split(inner);

    // ── tool line ──
    let tool_line = match (&ctx.active_tool, &ctx.active_tool_target) {
        (Some(name), Some(target)) if !target.is_empty() => Line::from(vec![
            pill(" TOOL ", Color::Yellow, Color::Black),
            Span::raw(" "),
            Span::styled(
                name.clone(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" · ", Style::default().fg(Color::DarkGray)),
            Span::styled(one_line(target), Style::default().fg(Color::White)),
        ]),
        (Some(name), _) => Line::from(vec![
            pill(" TOOL ", Color::Yellow, Color::Black),
            Span::raw(" "),
            Span::styled(
                name.clone(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        (None, _) => Line::from(vec![
            pill(" TOOL ", Color::DarkGray, Color::White),
            Span::raw(" "),
            Span::styled(
                "idle",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ),
        ]),
    };

    // ── about line ──
    let topic_value = one_line(ctx.topic.as_deref().unwrap_or("(waiting for first prompt)"));
    let about_line = Line::from(vec![
        pill("ABOUT ", Color::LightMagenta, Color::Black),
        Span::raw(" "),
        Span::styled(topic_value, Style::default().fg(Color::White)),
    ]);

    // ── trail line ──
    let trail_spans = build_trail_spans(ctx);

    // ── you line ──
    let you_line = Line::from(vec![
        pill("  YOU ", Color::Cyan, Color::Black),
        Span::raw(" "),
        Span::styled(
            one_line(ctx.last_user.as_deref().unwrap_or("(waiting)")),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::ITALIC),
        ),
    ]);

    f.render_widget(Paragraph::new(tool_line), chunks[0]);
    f.render_widget(
        Paragraph::new(about_line).wrap(Wrap { trim: false }),
        chunks[1],
    );
    f.render_widget(
        Paragraph::new(Line::from(trail_spans)).wrap(Wrap { trim: false }),
        chunks[2],
    );
    f.render_widget(Paragraph::new(you_line), chunks[3]);
}

fn render_ssh_panel(f: &mut Frame, pane: &Pane, ssh: &crate::ssh::SshContext, area: Rect) {
    let who = match (&ssh.user, &ssh.host) {
        (Some(u), h) => format!("{}@{}", u, h),
        (None, h) => h.clone(),
    };
    let display = ssh.display_name.clone().unwrap_or_else(|| who.clone());
    let ip = ssh.resolved_ip();
    let port_str = ssh.port.map(|p| p.to_string());
    let age = crate::ssh::format_duration(ssh.connection_age());

    // --- bordered block with title ------------------------------------------
    let mut title_spans: Vec<Span<'static>> = vec![
        Span::raw(" "),
        Span::styled(
            "🔐 ssh",
            Style::default()
                .fg(Color::LightBlue)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        dim_sep(),
        Span::raw(" "),
        Span::styled(
            display.clone(),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
    ];
    // If there's a custom alias AND it differs from user@host, show the real
    // target in dim text next to it so you never lose sight of what you're on.
    if ssh.display_name.is_some() && display != who {
        title_spans.push(Span::styled(
            format!(" ({})", who),
            Style::default().fg(Color::DarkGray),
        ));
    }
    title_spans.extend([
        Span::raw(" "),
        dim_sep(),
        Span::raw(" "),
        Span::styled("⏱ ", Style::default().fg(Color::Cyan)),
        Span::styled(
            format!("up {}", age),
            Style::default().fg(Color::Cyan),
        ),
    ]);
    if let Some(p) = &port_str {
        title_spans.push(Span::raw(" "));
        title_spans.push(dim_sep());
        title_spans.push(Span::raw(" "));
        title_spans.push(Span::styled(
            format!("port {}", p),
            Style::default().fg(Color::Yellow),
        ));
    }
    title_spans.push(Span::raw(" "));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Blue))
        .title(Line::from(title_spans))
        .title_alignment(Alignment::Left)
        .padding(Padding::horizontal(1));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // --- inner rows ---------------------------------------------------------
    let chunks = Layout::vertical([
        Constraint::Length(1), // host line
        Constraint::Length(1), // ip line
        Constraint::Length(1), // started at
        Constraint::Min(1),    // last command
        Constraint::Length(1), // remote command (if any) — else spacer
    ])
    .split(inner);

    let host_line = Line::from(vec![
        pill(" HOST ", Color::Blue, Color::White),
        Span::raw(" "),
        Span::styled(
            ssh.host.clone(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  user: {}", ssh.user.as_deref().unwrap_or("(default)")),
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    let ip_line = Line::from(vec![
        pill("   IP ", Color::LightBlue, Color::Black),
        Span::raw(" "),
        match ip {
            Some(ref addr) => Span::styled(
                addr.clone(),
                Style::default()
                    .fg(Color::LightBlue)
                    .add_modifier(Modifier::BOLD),
            ),
            None => Span::styled(
                "(resolving…)",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ),
        },
    ]);

    let started_display = ssh
        .started_at
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(format_local_time)
        .unwrap_or_else(|| "?".to_string());
    let started_line = Line::from(vec![
        pill("START ", Color::Magenta, Color::Black),
        Span::raw(" "),
        Span::styled(started_display, Style::default().fg(Color::White)),
    ]);

    let last_cmd = pane
        .last_typed
        .as_deref()
        .map(|s| one_line(s))
        .unwrap_or_else(|| "(none typed yet)".to_string());
    let last_line = Line::from(vec![
        pill(" LAST ", Color::Green, Color::Black),
        Span::raw(" "),
        Span::styled(
            last_cmd,
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
    ]);

    let remote_line = if let Some(cmd) = &ssh.remote_command {
        Line::from(vec![
            pill(" EXEC ", Color::Yellow, Color::Black),
            Span::raw(" "),
            Span::styled(
                one_line(cmd),
                Style::default().fg(Color::Yellow),
            ),
        ])
    } else {
        Line::from("")
    };

    f.render_widget(Paragraph::new(host_line), chunks[0]);
    f.render_widget(Paragraph::new(ip_line), chunks[1]);
    f.render_widget(Paragraph::new(started_line), chunks[2]);
    f.render_widget(
        Paragraph::new(last_line).wrap(Wrap { trim: false }),
        chunks[3],
    );
    f.render_widget(Paragraph::new(remote_line), chunks[4]);
}

#[cfg(target_os = "macos")]
fn render_service_panel(
    f: &mut Frame,
    svc: &crate::service::ServiceContext,
    area: Rect,
) {
    use crate::service::format_bytes;
    let uptime = crate::ssh::format_duration(svc.uptime());
    let first_port = svc.ports.first().copied();

    // --- title line ---------------------------------------------------------
    let mut title_spans: Vec<Span<'static>> = vec![
        Span::raw(" "),
        Span::styled(
            "🚀 service",
            Style::default()
                .fg(Color::LightGreen)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        dim_sep(),
        Span::raw(" "),
        Span::styled(
            svc.name.clone(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(port) = first_port {
        title_spans.push(Span::raw(" "));
        title_spans.push(dim_sep());
        title_spans.push(Span::raw(" "));
        title_spans.push(Span::styled(
            format!(":{}", port),
            Style::default()
                .fg(Color::LightGreen)
                .add_modifier(Modifier::BOLD),
        ));
    }
    title_spans.extend([
        Span::raw(" "),
        dim_sep(),
        Span::raw(" "),
        Span::styled("⏱ ", Style::default().fg(Color::Cyan)),
        Span::styled(format!("up {}", uptime), Style::default().fg(Color::Cyan)),
        Span::raw(" "),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::LightGreen))
        .title(Line::from(title_spans))
        .title_alignment(Alignment::Left)
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // --- inner layout -------------------------------------------------------
    let chunks = Layout::vertical([
        Constraint::Length(1), // ports
        Constraint::Length(1), // pid / proc
        Constraint::Length(1), // mem
        Constraint::Length(1), // cpu bar
        Constraint::Min(1),    // command (wraps)
    ])
    .split(inner);

    // ── ports ──
    let ports_text = if svc.ports.is_empty() {
        "(none)".to_string()
    } else {
        svc.ports
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let ports_line = Line::from(vec![
        pill(" PORT ", Color::LightGreen, Color::Black),
        Span::raw(" "),
        Span::styled(
            ports_text,
            Style::default()
                .fg(Color::LightGreen)
                .add_modifier(Modifier::BOLD),
        ),
    ]);

    // ── pid ──
    let pid_line = Line::from(vec![
        pill("  PID ", Color::Cyan, Color::Black),
        Span::raw(" "),
        Span::styled(
            svc.pid.to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", svc.name),
            Style::default().fg(Color::White),
        ),
    ]);

    // ── memory ──
    let mem_line = Line::from(vec![
        pill("  MEM ", Color::Magenta, Color::Black),
        Span::raw(" "),
        Span::styled(
            format!("rss {}", format_bytes(svc.rss_bytes)),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("vsz {}", format_bytes(svc.virtual_bytes)),
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    // ── cpu bar (20 segments) ──
    let cpu_bar_width: usize = 20;
    let filled = ((svc.cpu_pct / 100.0) * cpu_bar_width as f32)
        .round()
        .clamp(0.0, cpu_bar_width as f32) as usize;
    let bar_color = if svc.cpu_pct < 30.0 {
        Color::Green
    } else if svc.cpu_pct < 70.0 {
        Color::Yellow
    } else {
        Color::Red
    };
    let bar_filled: String = "█".repeat(filled);
    let bar_empty: String = "░".repeat(cpu_bar_width.saturating_sub(filled));
    let cpu_line = Line::from(vec![
        pill("  CPU ", Color::Yellow, Color::Black),
        Span::raw(" "),
        Span::styled(bar_filled, Style::default().fg(bar_color)),
        Span::styled(bar_empty, Style::default().fg(Color::DarkGray)),
        Span::raw("  "),
        Span::styled(
            format!("{:>5.1}%", svc.cpu_pct),
            Style::default().fg(bar_color).add_modifier(Modifier::BOLD),
        ),
    ]);

    // ── command ──
    let cmd_line = Line::from(vec![
        pill("  CMD ", Color::Blue, Color::White),
        Span::raw(" "),
        Span::styled(
            one_line(&svc.command),
            Style::default().fg(Color::White),
        ),
    ]);

    f.render_widget(Paragraph::new(ports_line), chunks[0]);
    f.render_widget(Paragraph::new(pid_line), chunks[1]);
    f.render_widget(Paragraph::new(mem_line), chunks[2]);
    f.render_widget(Paragraph::new(cpu_line), chunks[3]);
    f.render_widget(
        Paragraph::new(cmd_line).wrap(Wrap { trim: false }),
        chunks[4],
    );
}

fn format_local_time(since_epoch: std::time::Duration) -> String {
    // Best-effort local time string without pulling in chrono. We convert
    // epoch seconds + the system timezone offset (via libc::localtime_r).
    let secs = since_epoch.as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&secs, &mut tm).is_null() };
    if !ok {
        return "?".to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
    )
}

fn render_settings_modal(f: &mut Frame, app: &App, state: &SettingsState, area: Rect) {
    let width = 60u16.min(area.width.saturating_sub(4));
    let height = 10u16.min(area.height.saturating_sub(2));
    let rect = center_rect(width, height, area);

    // Clear the pane content behind the popup so it reads cleanly.
    f.render_widget(Clear, rect);

    let focused_pane = app.panes.get(app.focus);
    let subtitle = focused_pane
        .and_then(|p| p.initial_cwd.as_ref())
        .map(|p| shorten_home(p))
        .unwrap_or_else(|| "(no cwd)".to_string());

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Magenta))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "⚙ Pane Settings",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ]))
        .title_alignment(Alignment::Left)
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let chunks = Layout::vertical([
        Constraint::Length(1), // subtitle
        Constraint::Length(1), // spacer
        Constraint::Length(1), // name label
        Constraint::Length(1), // name field
        Constraint::Length(1), // spacer
        Constraint::Length(1), // color label
        Constraint::Length(1), // color field
        Constraint::Min(1),    // footer help
    ])
    .split(inner);

    let subtitle_line = Line::from(vec![
        Span::styled("cwd: ", Style::default().fg(Color::DarkGray)),
        Span::styled(subtitle, Style::default().fg(Color::White)),
    ]);
    f.render_widget(Paragraph::new(subtitle_line), chunks[0]);

    // Name field
    let name_focused = matches!(state.field, SettingsField::Name);
    let name_label_style = if name_focused {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    f.render_widget(
        Paragraph::new(Span::styled("Name", name_label_style)),
        chunks[2],
    );
    let name_value = if state.name_buffer.is_empty() {
        "(unnamed)".to_string()
    } else {
        state.name_buffer.clone()
    };
    let cursor = if name_focused { "▍" } else { "" };
    let name_field = Line::from(vec![
        Span::styled(
            " › ",
            if name_focused {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ),
        Span::styled(
            name_value,
            if state.name_buffer.is_empty() {
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC)
            } else {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            },
        ),
        Span::styled(cursor, Style::default().fg(Color::Yellow)),
    ]);
    f.render_widget(Paragraph::new(name_field), chunks[3]);

    // Color field
    let color_focused = matches!(state.field, SettingsField::Color);
    let color_label_style = if color_focused {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    f.render_widget(
        Paragraph::new(Span::styled("Accent Color", color_label_style)),
        chunks[5],
    );
    let color_name = state.current_color_name();
    let color_val = theme::color_from_name(color_name).unwrap_or(Color::Blue);
    let (lchev, rchev) = if color_focused {
        ("◀", "▶")
    } else {
        (" ", " ")
    };
    let color_field = Line::from(vec![
        Span::styled(
            " › ",
            if color_focused {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ),
        Span::styled(lchev, Style::default().fg(Color::Yellow)),
        Span::raw(" "),
        Span::styled("  ", Style::default().bg(color_val)),
        Span::raw(" "),
        Span::styled(
            color_name.to_string(),
            Style::default()
                .fg(color_val)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(rchev, Style::default().fg(Color::Yellow)),
    ]);
    f.render_widget(Paragraph::new(color_field), chunks[6]);

    let footer = Line::from(vec![
        Span::styled("Tab", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::styled(" switch · ", Style::default().fg(Color::DarkGray)),
        Span::styled("← →", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::styled(" color · ", Style::default().fg(Color::DarkGray)),
        Span::styled("Enter", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::styled(" save · ", Style::default().fg(Color::DarkGray)),
        Span::styled("Esc", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(footer), chunks[7]);
}

fn render_pane_picker_modal(f: &mut Frame, app: &App, state: &PanePickerState, area: Rect) {
    let row_count = app.panes.len().max(1) as u16;
    // 8 fixed rows of chrome around the list: 2 borders + 2 vertical padding
    // + subtitle + spacer + spacer + footer. Shrinking this below 8 clips
    // entries off the bottom of the list.
    let desired_h = (row_count + 8).min(area.height.saturating_sub(2));
    let width = 72u16.min(area.width.saturating_sub(4));
    let height = desired_h.max(8);
    let rect = center_rect(width, height, area);

    f.render_widget(Clear, rect);

    let visible_count = app.visible_panes().len();
    let total = app.panes.len();
    let title = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            "▦ Panes",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        dim_sep(),
        Span::raw(" "),
        Span::styled(
            format!("showing {} of {}", visible_count, total),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw(" "),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Magenta))
        .title(title)
        .title_alignment(Alignment::Left)
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let chunks = Layout::vertical([
        Constraint::Length(1), // subtitle
        Constraint::Length(1), // spacer
        Constraint::Min(1),    // pane list
        Constraint::Length(1), // spacer
        Constraint::Length(1), // footer
    ])
    .split(inner);

    let subtitle = Line::from(vec![
        Span::styled(
            "Pick a pane to place it in the leftmost slot.",
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    f.render_widget(Paragraph::new(subtitle), chunks[0]);

    let visible_set: std::collections::HashSet<usize> =
        app.visible_panes().into_iter().collect();

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(app.panes.len());
    for (i, pane) in app.panes.iter().enumerate() {
        let selected = i == state.cursor;
        let is_primary = app.has_overflow() && i == app.primary;
        let is_visible = visible_set.contains(&i);

        let accent = pane
            .settings
            .color
            .as_deref()
            .and_then(theme::color_from_name)
            .unwrap_or(Color::Blue);

        let name = pane
            .settings
            .name
            .clone()
            .unwrap_or_else(|| format!("pane {}", i + 1));
        let cmd = pane
            .proc_info
            .as_ref()
            .map(|p| p.display_name().to_string())
            .unwrap_or_else(|| "…".to_string());

        let cursor_marker = if selected { "▶ " } else { "  " };
        let visibility_marker = if is_primary {
            "★"
        } else if is_visible {
            "●"
        } else {
            "·"
        };
        let visibility_style = if is_primary {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else if is_visible {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let row_style = if selected {
            Style::default()
                .bg(Color::Rgb(0x44, 0x3a, 0x78))
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        let spans = vec![
            Span::styled(
                cursor_marker,
                if selected {
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(format!("{} ", i + 1), Style::default().fg(Color::DarkGray)),
            Span::styled(visibility_marker, visibility_style),
            Span::raw(" "),
            Span::styled("  ", Style::default().bg(accent)),
            Span::raw(" "),
            Span::styled(
                name,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
            Span::styled(cmd, Style::default().fg(Color::Cyan)),
        ];
        lines.push(Line::from(spans).style(row_style));
    }

    f.render_widget(Paragraph::new(lines), chunks[2]);

    let footer = Line::from(vec![
        Span::styled("↑/↓", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::styled(" move · ", Style::default().fg(Color::DarkGray)),
        Span::styled("1-9", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::styled(" quick-select · ", Style::default().fg(Color::DarkGray)),
        Span::styled("Enter", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::styled(" show · ", Style::default().fg(Color::DarkGray)),
        Span::styled("Esc", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(footer), chunks[4]);
}

fn center_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}

fn dim_sep() -> Span<'static> {
    Span::styled("·", Style::default().fg(Color::DarkGray))
}

/// Small "pill" label: colored background, bold label text.
fn pill(label: &str, bg: Color, fg: Color) -> Span<'static> {
    Span::styled(
        label.to_string(),
        Style::default()
            .bg(bg)
            .fg(fg)
            .add_modifier(Modifier::BOLD),
    )
}

fn build_trail_spans(ctx: &crate::claude::ClaudeContext) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(ctx.recent_tools.len() * 4 + 2);
    spans.push(pill("TRAIL ", Color::Green, Color::Black));
    spans.push(Span::raw(" "));

    if ctx.recent_tools.is_empty() {
        spans.push(Span::styled(
            "(no tools yet)",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ));
        return spans;
    }

    let mut first = true;
    for (name, target) in ctx.recent_tools.iter() {
        if !first {
            spans.push(Span::styled(" → ", Style::default().fg(Color::DarkGray)));
        }
        first = false;
        spans.push(Span::styled(
            name.clone(),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
        if let Some(t) = target {
            if !t.is_empty() {
                spans.push(Span::styled(" ", Style::default()));
                spans.push(Span::styled(
                    one_line(t),
                    Style::default().fg(Color::White),
                ));
            }
        }
    }
    spans
}

fn one_line(s: &str) -> String {
    let compact: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c })
        .collect();
    // Collapse runs of whitespace so wrapped output doesn't contain visual gaps.
    let mut out = String::with_capacity(compact.len());
    let mut prev_space = false;
    for c in compact.chars() {
        if c == ' ' {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

fn pane_header_label(pane: &Pane, idx: usize, focused: bool, width: usize) -> String {
    let marker = if focused { "●" } else { "○" };
    let (cmd, cwd) = match &pane.proc_info {
        Some(info) => (
            info.display_name(),
            info.cwd.as_ref().map(|p| shorten_home(p)),
        ),
        None => ("…", None),
    };
    let pane_label = pane
        .settings
        .name
        .clone()
        .unwrap_or_else(|| format!("pane {}", idx + 1));
    let base = match cwd {
        Some(dir) => format!(" {} {} · {} · {} ", marker, pane_label, cmd, dir),
        None => format!(" {} {} · {} ", marker, pane_label, cmd),
    };
    if base.chars().count() > width && width > 2 {
        let mut out: String = base.chars().take(width.saturating_sub(1)).collect();
        out.push('…');
        out
    } else {
        base
    }
}

fn shorten_home(path: &std::path::Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rest) = path.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

fn cell_to_style(fg: AlacColor, bg: AlacColor, flags: Flags) -> Style {
    let mut style = Style::default();
    if let Some(c) = alac_color_to_ratatui(fg, true) {
        style = style.fg(c);
    }
    if let Some(c) = alac_color_to_ratatui(bg, false) {
        style = style.bg(c);
    }
    if flags.contains(Flags::BOLD) {
        style = style.add_modifier(Modifier::BOLD);
    }
    if flags.contains(Flags::ITALIC) {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if flags.contains(Flags::UNDERLINE) {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if flags.contains(Flags::INVERSE) {
        style = style.add_modifier(Modifier::REVERSED);
    }
    if flags.contains(Flags::DIM) {
        style = style.add_modifier(Modifier::DIM);
    }
    style
}

fn alac_color_to_ratatui(c: AlacColor, _is_fg: bool) -> Option<Color> {
    match c {
        AlacColor::Named(n) => named_to_ratatui(n),
        AlacColor::Spec(rgb) => Some(Color::Rgb(rgb.r, rgb.g, rgb.b)),
        AlacColor::Indexed(i) => Some(ansi_index_to_color(i)),
    }
}

fn named_to_ratatui(n: NamedColor) -> Option<Color> {
    use NamedColor::*;
    Some(match n {
        // Let the host terminal's default fg/bg show through.
        Background | Foreground | DimForeground | BrightForeground => return None,
        Black => Color::Black,
        Red => Color::Red,
        Green => Color::Green,
        Yellow => Color::Yellow,
        Blue => Color::Blue,
        Magenta => Color::Magenta,
        Cyan => Color::Cyan,
        White => Color::Gray,
        BrightBlack => Color::DarkGray,
        BrightRed => Color::LightRed,
        BrightGreen => Color::LightGreen,
        BrightYellow => Color::LightYellow,
        BrightBlue => Color::LightBlue,
        BrightMagenta => Color::LightMagenta,
        BrightCyan => Color::LightCyan,
        BrightWhite => Color::White,
        _ => return None,
    })
}

fn ansi_index_to_color(i: u8) -> Color {
    match i {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        8 => Color::DarkGray,
        9 => Color::LightRed,
        10 => Color::LightGreen,
        11 => Color::LightYellow,
        12 => Color::LightBlue,
        13 => Color::LightMagenta,
        14 => Color::LightCyan,
        15 => Color::White,
        _ => Color::Indexed(i),
    }
}
