use std::collections::VecDeque;
use std::fs;
use std::io;
use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use serde_json::Value;
use tokio::sync::broadcast;

use crate::config::{Config, ServeArgs};
use crate::event::EventRecord;
use crate::server_runner::ManagedServer;

const ACTION_COUNT: usize = 7;
const MAX_LOG_LINES: usize = 256;
const MAX_LOG_EVENTS: usize = 256;
const LOG_SCROLL_STEP: usize = 6;

pub async fn run(args: ServeArgs) -> Result<()> {
    let config = Config::load(args).context("failed to load configuration")?;
    let mut terminal = init_terminal()?;
    let _guard = TerminalGuard;
    let mut app = TuiApp::new(config);

    loop {
        app.refresh_server_status().await;
        app.drain_live_events();
        terminal.draw(|frame| app.render(frame))?;

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) => {
                    if app.handle_key(key).await? {
                        break;
                    }
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => app.scroll_logs_up(LOG_SCROLL_STEP),
                    MouseEventKind::ScrollDown => app.scroll_logs_down(LOG_SCROLL_STEP),
                    _ => {}
                },
                _ => {}
            }
        }
    }

    app.stop_server().await?;
    Ok(())
}

fn init_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    execute!(io::stdout(), EnableMouseCapture)?;
    match Terminal::new(CrosstermBackend::new(stdout)) {
        Ok(mut terminal) => {
            terminal.clear()?;
            Ok(terminal)
        }
        Err(error) => {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), DisableMouseCapture);
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
            Err(error).context("failed to initialize terminal")
        }
    }
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableMouseCapture);
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

struct TuiApp {
    config: Config,
    server: Option<ManagedServer>,
    selected_action: usize,
    input: Option<InputMode>,
    last_error: Option<String>,
    notice: String,
    live_logs: VecDeque<String>,
    live_events: VecDeque<EventRecord>,
    log_scroll: usize,
    log_follow_tail: bool,
    event_rx: Option<broadcast::Receiver<EventRecord>>,
}

impl TuiApp {
    fn new(config: Config) -> Self {
        Self {
            config,
            server: None,
            selected_action: 0,
            input: None,
            last_error: None,
            notice: "Service is stopped. Select Start when ready.".to_owned(),
            live_logs: VecDeque::new(),
            live_events: VecDeque::new(),
            log_scroll: 0,
            log_follow_tail: true,
            event_rx: None,
        }
    }

    fn drain_live_events(&mut self) {
        loop {
            let result = match self.event_rx.as_mut() {
                Some(event_rx) => event_rx.try_recv(),
                None => return,
            };

            match result {
                Ok(event) => {
                    self.push_log(summarize_event(&event));
                    self.push_event(event);
                }
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                    self.push_log(format!("Live event stream skipped {skipped} stale events."));
                }
                Err(broadcast::error::TryRecvError::Closed) => {
                    self.push_log("Live event stream closed.".to_owned());
                    self.event_rx = None;
                    break;
                }
            }
        }
    }

    fn push_event(&mut self, event: EventRecord) {
        if self.live_events.len() >= MAX_LOG_EVENTS {
            self.live_events.pop_front();
        }
        self.live_events.push_back(event);
    }

    fn push_log(&mut self, line: String) {
        if self.live_logs.len() >= MAX_LOG_LINES {
            self.live_logs.pop_front();
            if !self.log_follow_tail {
                self.log_scroll = self.log_scroll.saturating_sub(1);
            }
        }
        self.live_logs.push_back(line);
        if self.log_follow_tail {
            self.log_scroll = self.bottom_log_scroll();
        }
    }

    fn visible_log_lines(&self) -> usize {
        12
    }

    fn bottom_log_scroll(&self) -> usize {
        self.live_logs
            .len()
            .saturating_sub(self.visible_log_lines())
    }

    fn scroll_logs_up(&mut self, amount: usize) {
        self.log_follow_tail = false;
        self.log_scroll = self.log_scroll.saturating_sub(amount);
    }

    fn scroll_logs_down(&mut self, amount: usize) {
        let max_scroll = self.bottom_log_scroll();
        self.log_scroll = (self.log_scroll + amount).min(max_scroll);
        self.log_follow_tail = self.log_scroll >= max_scroll;
    }

    fn scroll_logs_to_top(&mut self) {
        self.log_follow_tail = false;
        self.log_scroll = 0;
    }

    fn scroll_logs_to_bottom(&mut self) {
        self.log_follow_tail = true;
        self.log_scroll = self.bottom_log_scroll();
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if self.input.is_some() {
            self.handle_input_key(key).await?;
            return Ok(false);
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected_action = self.selected_action.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected_action = (self.selected_action + 1).min(ACTION_COUNT - 1);
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if self.run_selected_action().await? {
                    return Ok(true);
                }
            }
            KeyCode::Char('s') => self.toggle_server().await?,
            KeyCode::Char('r') => self.restart_server().await?,
            KeyCode::Char('h') => self.start_host_edit(),
            KeyCode::Char('p') => self.start_port_edit(),
            KeyCode::Char('w') => self.save_config()?,
            KeyCode::Char('l') => self.save_logs()?,
            KeyCode::PageUp => self.scroll_logs_up(LOG_SCROLL_STEP),
            KeyCode::PageDown => self.scroll_logs_down(LOG_SCROLL_STEP),
            KeyCode::Home => self.scroll_logs_to_top(),
            KeyCode::End => self.scroll_logs_to_bottom(),
            _ => {}
        }

        Ok(false)
    }

    async fn handle_input_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => {
                self.input = None;
                self.notice = "Edit canceled.".to_owned();
            }
            KeyCode::Enter => self.commit_input().await?,
            KeyCode::Backspace => {
                if let Some(input) = &mut self.input {
                    input.value.pop();
                }
            }
            KeyCode::Char(ch) => {
                if let Some(input) = &mut self.input {
                    input.value.push(ch);
                }
            }
            _ => {}
        }

        Ok(())
    }

    async fn run_selected_action(&mut self) -> Result<bool> {
        match self.selected_action {
            0 => self.toggle_server().await?,
            1 => self.restart_server().await?,
            2 => self.start_host_edit(),
            3 => self.start_port_edit(),
            4 => self.save_config()?,
            5 => self.save_logs()?,
            6 => return Ok(true),
            _ => {}
        }
        Ok(false)
    }

    async fn toggle_server(&mut self) -> Result<()> {
        if self.server.is_some() {
            self.stop_server().await
        } else {
            self.start_server().await
        }
    }

    async fn start_server(&mut self) -> Result<()> {
        if self.server.is_some() {
            self.notice = "Service is already running.".to_owned();
            return Ok(());
        }

        let mut config = self.config.clone();
        config.host = self.config.host.trim().to_owned();
        config.port = self.config.port;
        self.notice = "Starting service...".to_owned();
        match ManagedServer::start(config).await {
            Ok(server) => {
                self.notice = format!("Service started on {}.", server.addr());
                self.last_error = None;
                self.push_log(self.notice.clone());
                self.event_rx = Some(server.subscribe_events());
                self.server = Some(server);
            }
            Err(error) => {
                self.last_error = Some(error.to_string());
                self.notice = "Service failed to start.".to_owned();
                self.push_log(format!("{} {error}", self.notice.clone()));
            }
        }
        Ok(())
    }

    async fn stop_server(&mut self) -> Result<()> {
        let Some(server) = self.server.take() else {
            self.notice = "Service is already stopped.".to_owned();
            return Ok(());
        };

        self.notice = "Stopping service...".to_owned();
        self.push_log(self.notice.clone());
        match server.stop().await {
            Ok(()) => {
                self.notice = "Service stopped.".to_owned();
                self.last_error = None;
                self.push_log(self.notice.clone());
            }
            Err(error) => {
                self.notice = "Service stopped with an error.".to_owned();
                self.last_error = Some(error.to_string());
                self.push_log(format!("{} {}", self.notice.clone(), error));
            }
        }
        self.drain_live_events();
        self.event_rx = None;
        Ok(())
    }

    async fn restart_server(&mut self) -> Result<()> {
        if self.server.is_some() {
            self.stop_server().await?;
        }
        self.start_server().await
    }

    async fn refresh_server_status(&mut self) {
        let Some(server) = self.server.as_ref() else {
            return;
        };
        if !server.is_finished() {
            return;
        }
        if let Some(server) = self.server.take() {
            match server.wait().await {
                Ok(()) => {
                    self.notice = "Service exited.".to_owned();
                    self.last_error = None;
                    self.push_log(self.notice.clone());
                }
                Err(error) => {
                    self.notice = "Service exited unexpectedly.".to_owned();
                    self.last_error = Some(error.to_string());
                    self.push_log(format!("{} {}", self.notice.clone(), error));
                }
            }
            self.drain_live_events();
            self.event_rx = None;
        }
    }

    fn start_host_edit(&mut self) {
        self.input = Some(InputMode {
            field: InputField::Host,
            value: self.config.host.clone(),
        });
        self.notice = "Editing host. Press Enter to apply or Esc to cancel.".to_owned();
    }

    fn start_port_edit(&mut self) {
        self.input = Some(InputMode {
            field: InputField::Port,
            value: self.config.port.to_string(),
        });
        self.notice = "Editing port. Press Enter to apply or Esc to cancel.".to_owned();
    }

    async fn commit_input(&mut self) -> Result<()> {
        let Some(input) = self.input.take() else {
            return Ok(());
        };

        match input.field {
            InputField::Host => match validate_host(&input.value) {
                Ok(host) => {
                    self.config.host = host;
                    self.notice = "Host updated. Save config to persist it.".to_owned();
                    self.last_error = None;
                }
                Err(error) => {
                    self.last_error = Some(error);
                    self.notice = "Host was not changed.".to_owned();
                }
            },
            InputField::Port => match validate_port(&input.value) {
                Ok(port) => {
                    self.config.port = port;
                    self.notice = "Port updated. Save config to persist it.".to_owned();
                    self.last_error = None;
                }
                Err(error) => {
                    self.last_error = Some(error);
                    self.notice = "Port was not changed.".to_owned();
                }
            },
        }

        Ok(())
    }

    fn save_config(&mut self) -> Result<()> {
        Config::save_host_port(
            self.config.data_dir.clone(),
            &self.config.host,
            self.config.port,
        )?;
        self.notice = format!(
            "Saved host and port to {}/config.toml.",
            self.config.data_dir.display()
        );
        self.last_error = None;
        Ok(())
    }

    fn save_logs(&mut self) -> Result<()> {
        let dir = self.config.data_dir.join("tui-logs");
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create log directory {}", dir.display()))?;
        let timestamp = Utc::now().format("%Y%m%d-%H%M%S");
        let text_path = dir.join(format!("todex-tui-{timestamp}.log"));
        let jsonl_path = dir.join(format!("todex-tui-{timestamp}.jsonl"));

        let runtime_config = self
            .server
            .as_ref()
            .map(ManagedServer::config)
            .unwrap_or(&self.config);
        let mut text = String::new();
        text.push_str("TodeX TUI log export\n");
        text.push_str(&format!("exported_at={}\n", Utc::now().to_rfc3339()));
        text.push_str(&format!(
            "listen={}:{}\n",
            runtime_config.host, runtime_config.port
        ));
        text.push_str(&format!("data_dir={}\n", runtime_config.data_dir.display()));
        text.push_str(&format!(
            "workspace_root={}\n",
            runtime_config.workspace_root.display()
        ));
        text.push_str(&format!("notice={}\n", self.notice));
        if let Some(error) = &self.last_error {
            text.push_str(&format!("last_error={error}\n"));
        }
        text.push('\n');
        for line in &self.live_logs {
            text.push_str(line);
            text.push('\n');
        }

        let mut jsonl = String::new();
        for event in &self.live_events {
            jsonl.push_str(&serde_json::to_string(event)?);
            jsonl.push('\n');
        }

        fs::write(&text_path, text)
            .with_context(|| format!("failed to write {}", text_path.display()))?;
        fs::write(&jsonl_path, jsonl)
            .with_context(|| format!("failed to write {}", jsonl_path.display()))?;

        self.notice = format!(
            "Saved logs to {} and {}.",
            text_path.display(),
            jsonl_path.display()
        );
        self.last_error = None;
        self.push_log(self.notice.clone());
        Ok(())
    }

    fn render(&self, frame: &mut Frame<'_>) {
        frame.render_widget(Clear, frame.area());
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(11),
                Constraint::Min(10),
                Constraint::Length(5),
                Constraint::Length(4),
            ])
            .split(frame.area());
        let main_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(40), Constraint::Length(32)])
            .split(chunks[1]);

        for area in [
            chunks[0],
            main_chunks[0],
            main_chunks[1],
            chunks[2],
            chunks[3],
        ] {
            frame.render_widget(Clear, area);
        }

        frame.render_widget(self.status_panel(), chunks[0]);
        frame.render_widget(self.log_panel(main_chunks[0]), main_chunks[0]);
        frame.render_widget(self.action_panel(), main_chunks[1]);
        frame.render_widget(self.help_panel(), chunks[2]);
        frame.render_widget(self.message_panel(), chunks[3]);
    }

    fn status_panel(&self) -> Paragraph<'_> {
        let status = if self.server.is_some() {
            Span::styled("Running", Style::default().fg(Color::Green))
        } else if self.last_error.is_some() {
            Span::styled("Failed", Style::default().fg(Color::Red))
        } else {
            Span::styled("Stopped", Style::default().fg(Color::Yellow))
        };
        let uptime = self
            .server
            .as_ref()
            .map(|server| format_duration(server.started_at().elapsed()))
            .unwrap_or_else(|| "-".to_owned());
        let adapters = self
            .server
            .as_ref()
            .map(ManagedServer::active_codex_adapters)
            .unwrap_or(0);
        let connections = self
            .server
            .as_ref()
            .map(ManagedServer::websocket_connection_count)
            .unwrap_or(0);
        let connection_state = if self.server.is_none() {
            Span::styled("offline", Style::default().fg(Color::Yellow))
        } else if connections > 0 {
            Span::styled(
                format!("connected ({connections} active)"),
                Style::default().fg(Color::Green),
            )
        } else {
            Span::styled("listening (0 active)", Style::default().fg(Color::Cyan))
        };
        let runtime_config = self
            .server
            .as_ref()
            .map(ManagedServer::config)
            .unwrap_or(&self.config);
        let token = runtime_config
            .security
            .auth_token
            .as_deref()
            .filter(|token| !token.trim().is_empty());
        let token_line = match token {
            Some(token) => Line::from(vec![
                Span::raw("Token: "),
                Span::styled(token.to_owned(), Style::default().fg(Color::Green)),
            ]),
            None => Line::from(vec![
                Span::raw("Token: "),
                Span::styled("not set", Style::default().fg(Color::Red)),
            ]),
        };
        let auth_state = if runtime_config.security.enable_auth {
            Span::styled("enabled", Style::default().fg(Color::Yellow))
        } else {
            Span::styled("disabled", Style::default().fg(Color::Green))
        };

        Paragraph::new(vec![
            Line::from(vec![Span::raw("Status: "), status]),
            Line::from(format!(
                "Listen: {}:{}",
                runtime_config.host, runtime_config.port
            )),
            Line::from(format!(
                "WS endpoint: ws://{}:{}/v1/ws",
                runtime_config.host, runtime_config.port
            )),
            Line::from(vec![Span::raw("Auth: "), auth_state]),
            token_line,
            Line::from(format!("Data dir: {}", runtime_config.data_dir.display())),
            Line::from(format!(
                "Workspace root: {}",
                runtime_config.workspace_root.display()
            )),
            Line::from(format!(
                "Uptime: {} | Active Codex adapters: {}",
                uptime, adapters
            )),
            Line::from(vec![Span::raw("Connection: "), connection_state]),
        ])
        .block(
            Block::default()
                .title("TodeX Backend")
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: true })
    }

    fn log_panel(&self, area: ratatui::layout::Rect) -> Paragraph<'_> {
        let lines = if self.live_logs.is_empty() {
            vec![Line::from("No runtime events yet.")]
        } else {
            self.live_logs
                .iter()
                .map(|line| Line::from(line.clone()))
                .collect::<Vec<_>>()
        };
        let content_height = area.height.saturating_sub(2) as usize;
        let max_scroll = lines.len().saturating_sub(content_height);
        let scroll = if self.log_follow_tail {
            max_scroll
        } else {
            self.log_scroll.min(max_scroll)
        };
        let title = if self.log_follow_tail {
            format!("Live Logs [follow {}]", lines.len())
        } else {
            format!("Live Logs [{} / {}]", scroll, lines.len())
        };

        Paragraph::new(lines)
            .scroll((scroll as u16, 0))
            .wrap(Wrap { trim: true })
            .block(Block::default().title(title).borders(Borders::ALL))
    }

    fn action_panel(&self) -> List<'_> {
        let start_stop = if self.server.is_some() {
            "Stop service"
        } else {
            "Start service"
        };
        let actions = [
            start_stop,
            "Restart service",
            "Edit listen IP",
            "Edit listen port",
            "Save IP and port",
            "Save logs",
            "Quit",
        ];
        let items = actions
            .iter()
            .enumerate()
            .map(|(idx, label)| {
                let marker = if idx == self.selected_action {
                    "> "
                } else {
                    "  "
                };
                let style = if idx == self.selected_action {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(format!("{marker}{label}"))).style(style)
            })
            .collect::<Vec<_>>();

        List::new(items).block(Block::default().title("Actions").borders(Borders::ALL))
    }

    fn help_panel(&self) -> Paragraph<'_> {
        let input_line = self.input.as_ref().map(|input| {
            let name = match input.field {
                InputField::Host => "Host",
                InputField::Port => "Port",
            };
            Line::from(vec![
                Span::styled(
                    format!("{name}: "),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(input.value.clone()),
            ])
        });
        let lines = if let Some(input_line) = input_line {
            vec![
                input_line,
                Line::from("Enter applies the value. Esc cancels the edit."),
            ]
        } else {
            vec![
                Line::from("Use Up/Down or j/k to choose an action, Enter to run it."),
                Line::from(
                    "Shortcuts: s start/stop, r restart, h host, p port, w config, l logs, q quit.",
                ),
                Line::from("The TUI starts stopped by default and stops its service on exit."),
            ]
        };

        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(Block::default().title("Controls").borders(Borders::ALL))
    }

    fn message_panel(&self) -> Paragraph<'_> {
        let mut lines = vec![Line::from(self.notice.clone())];
        if let Some(error) = &self.last_error {
            lines.push(Line::from(vec![
                Span::styled("Error: ", Style::default().fg(Color::Red)),
                Span::raw(error.clone()),
            ]));
        }

        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(Block::default().title("Messages").borders(Borders::ALL))
    }
}

fn summarize_event(event: &EventRecord) -> String {
    let timestamp = event.time.format("%H:%M:%S");
    let detail = summarize_value(&event.payload);
    if detail.is_empty() {
        format!("[{timestamp}] {}", event.event_type)
    } else {
        format!("[{timestamp}] {} {}", event.event_type, detail)
    }
}

fn summarize_value(value: &Value) -> String {
    let Some(object) = value.as_object() else {
        return compact_scalar(value);
    };

    let source = object
        .get("data")
        .and_then(Value::as_object)
        .unwrap_or(object);
    let mut fields = Vec::new();
    if let Some(error) = source
        .get("error")
        .or_else(|| object.get("error"))
        .and_then(Value::as_object)
    {
        if let Some(code) = error.get("code") {
            fields.push(format!("error.code={}", compact_scalar(code)));
        }
        if let Some(message) = error.get("message") {
            fields.push(format!("error.message={}", compact_scalar(message)));
        }
    }
    for key in [
        "requestId",
        "request_id",
        "codexSessionId",
        "codex_session_id",
        "threadId",
        "turnId",
        "active_connections",
        "authenticated",
        "lifecycleState",
        "status",
        "decision",
        "reason_code",
        "message",
    ] {
        if let Some(value) = source.get(key).or_else(|| object.get(key)) {
            fields.push(format!("{key}={}", compact_scalar(value)));
        }
    }

    if fields.is_empty() {
        if let Some(error) = source.get("error").or_else(|| object.get("error")) {
            return format!("error={}", compact_scalar(error));
        }
    }

    fields.join(" ")
}

fn compact_scalar(value: &Value) -> String {
    match value {
        Value::String(text) => truncate_text(text, 64),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Null => "null".to_owned(),
        Value::Array(items) => format!("{} items", items.len()),
        Value::Object(object) => {
            if let Some(message) = object.get("message").and_then(Value::as_str) {
                return truncate_text(message, 64);
            }
            if let Some(code) = object.get("code").and_then(Value::as_str) {
                return code.to_owned();
            }
            if let Some(status) = object.get("status").and_then(Value::as_str) {
                return status.to_owned();
            }
            format!("{} keys", object.len())
        }
    }
}

fn truncate_text(value: &str, limit: usize) -> String {
    let text = value.trim();
    if text.chars().count() > limit {
        let keep = limit.saturating_sub(3);
        let mut truncated = text.chars().take(keep).collect::<String>();
        truncated.push_str("...");
        truncated
    } else {
        text.to_owned()
    }
}

struct InputMode {
    field: InputField,
    value: String,
}

enum InputField {
    Host,
    Port,
}

fn validate_host(value: &str) -> std::result::Result<String, String> {
    let host = value.trim();
    if host.is_empty() {
        return Err("host cannot be empty".to_owned());
    }
    host.parse::<IpAddr>()
        .map(|_| host.to_owned())
        .map_err(|_| "host must be a valid IPv4 or IPv6 address".to_owned())
}

fn validate_port(value: &str) -> std::result::Result<u16, String> {
    let port = value
        .trim()
        .parse::<u16>()
        .map_err(|_| "port must be a number from 1 to 65535".to_owned())?;
    if port == 0 {
        return Err("port must be a number from 1 to 65535".to_owned());
    }
    Ok(port)
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::{validate_host, validate_port};

    #[test]
    fn validate_host_accepts_ip_addresses() {
        assert_eq!(validate_host("127.0.0.1").unwrap(), "127.0.0.1");
        assert_eq!(validate_host("::1").unwrap(), "::1");
    }

    #[test]
    fn validate_host_rejects_empty_or_hostname_values() {
        assert!(validate_host("").is_err());
        assert!(validate_host("localhost").is_err());
    }

    #[test]
    fn validate_port_accepts_valid_user_ports() {
        assert_eq!(validate_port("1").unwrap(), 1);
        assert_eq!(validate_port("65535").unwrap(), 65535);
    }

    #[test]
    fn validate_port_rejects_zero_and_invalid_values() {
        assert!(validate_port("0").is_err());
        assert!(validate_port("65536").is_err());
        assert!(validate_port("abc").is_err());
    }
}
