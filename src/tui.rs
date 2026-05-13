use std::collections::{BTreeMap, BTreeSet, VecDeque};
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
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use serde_json::Value;
use tokio::sync::broadcast;

use crate::config::{Config, PairingEncryption, ServeArgs};
use crate::event::EventRecord;
use crate::server_runner::ManagedServer;
use crate::transport_crypto::render_qr_text_for_bounds;

const ACTION_COUNT: usize = 9;
const MAX_LOG_LINES: usize = 256;
const LOG_SCROLL_STEP: usize = 6;
const QR_POPUP_MARGIN: u16 = 1;

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

struct PairingQr {
    payloads: Vec<String>,
    active_index: usize,
}

struct PairingQrPopup {
    area: Rect,
    lines: Vec<Line<'static>>,
    title: String,
}

struct TuiApp {
    config: Config,
    server: Option<ManagedServer>,
    view: TuiView,
    selected_action: usize,
    selected_session: usize,
    input: Option<InputMode>,
    last_error: Option<String>,
    notice: String,
    live_logs: VecDeque<String>,
    live_events: Vec<EventRecord>,
    log_scroll: usize,
    observer_scroll: usize,
    log_follow_tail: bool,
    pairing_qr: Option<PairingQr>,
    event_rx: Option<broadcast::Receiver<EventRecord>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TuiView {
    Control,
    Observer,
}

impl TuiApp {
    fn new(config: Config) -> Self {
        Self {
            config,
            server: None,
            view: TuiView::Control,
            selected_action: 0,
            selected_session: 0,
            input: None,
            last_error: None,
            notice: "Service is stopped. Select Start when ready.".to_owned(),
            live_logs: VecDeque::new(),
            live_events: Vec::new(),
            log_scroll: 0,
            observer_scroll: 0,
            log_follow_tail: true,
            pairing_qr: None,
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
        self.live_events.push(event);
        let session_count = self.observer_state().sessions.len();
        if session_count == 0 {
            self.selected_session = 0;
        } else {
            self.selected_session = self.selected_session.min(session_count - 1);
        }
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

    fn scroll_observer_up(&mut self, amount: usize) {
        self.observer_scroll = self.observer_scroll.saturating_sub(amount);
    }

    fn scroll_observer_down(&mut self, amount: usize) {
        self.observer_scroll = self.observer_scroll.saturating_add(amount);
    }

    fn scroll_observer_to_top(&mut self) {
        self.observer_scroll = 0;
    }

    fn show_pairing_qr(&mut self) {
        let qr = match self.server.as_ref() {
            Some(server) => server.pairing_qr_payloads(self.config.pairing_encryption),
            None => {
                self.notice = "Start the service before showing a pairing QR.".to_owned();
                return;
            }
        };
        match qr {
            Ok(payloads) => {
                let total = payloads.len();
                self.pairing_qr = Some(PairingQr {
                    payloads,
                    active_index: 0,
                });
                self.notice = if total > 1 {
                    format!("Pairing QR is open in the center window. Use Left/Right to switch {total} segments.")
                } else {
                    "Pairing QR is open in the center window.".to_owned()
                };
            }
            Err(error) => {
                self.notice = "Failed to render pairing QR.".to_owned();
                self.last_error = Some(error.to_string());
            }
        }
    }

    fn close_pairing_qr(&mut self) {
        if self.pairing_qr.is_some() {
            self.pairing_qr = None;
            self.notice = "Pairing QR closed.".to_owned();
        }
    }

    fn next_pairing_qr(&mut self) {
        if let Some(qr) = &mut self.pairing_qr {
            if !qr.payloads.is_empty() {
                qr.active_index = (qr.active_index + 1) % qr.payloads.len();
            }
        }
    }

    fn previous_pairing_qr(&mut self) {
        if let Some(qr) = &mut self.pairing_qr {
            if !qr.payloads.is_empty() {
                qr.active_index = if qr.active_index == 0 {
                    qr.payloads.len() - 1
                } else {
                    qr.active_index - 1
                };
            }
        }
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if self.pairing_qr.is_some() {
            match key.code {
                KeyCode::Left | KeyCode::PageUp => self.previous_pairing_qr(),
                KeyCode::Right | KeyCode::PageDown => self.next_pairing_qr(),
                KeyCode::Esc | KeyCode::Char('q') => self.close_pairing_qr(),
                _ => self.close_pairing_qr(),
            }
            return Ok(false);
        }

        if self.input.is_some() {
            self.handle_input_key(key).await?;
            return Ok(false);
        }

        if self.view == TuiView::Observer {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('o') | KeyCode::Tab => {
                    self.view = TuiView::Control;
                    self.notice = "Returned to the control view.".to_owned();
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.selected_session = self.selected_session.saturating_sub(1);
                    self.observer_scroll = 0;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let session_count = self.observer_state().sessions.len();
                    if session_count > 0 {
                        self.selected_session = (self.selected_session + 1).min(session_count - 1);
                    }
                    self.observer_scroll = 0;
                }
                KeyCode::PageUp => self.scroll_observer_up(LOG_SCROLL_STEP),
                KeyCode::PageDown => self.scroll_observer_down(LOG_SCROLL_STEP),
                KeyCode::Home => self.scroll_observer_to_top(),
                KeyCode::End => self.observer_scroll = usize::MAX,
                _ => {}
            }
            return Ok(false);
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
            KeyCode::Char('o') | KeyCode::Tab => {
                self.view = TuiView::Observer;
                self.notice = "Observer is read-only. Use q, Esc, o, or Tab to return.".to_owned();
            }
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
            KeyCode::Char('e') => self.cycle_pairing_encryption(),
            KeyCode::Char('w') => self.save_config()?,
            KeyCode::Char('l') => self.save_logs()?,
            KeyCode::Char('g') => self.show_pairing_qr(),
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
            4 => self.cycle_pairing_encryption(),
            5 => self.save_config()?,
            6 => self.show_pairing_qr(),
            7 => self.save_logs()?,
            8 => return Ok(true),
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
        Config::save_tui_settings(
            self.config.data_dir.clone(),
            &self.config.host,
            self.config.port,
            self.config.pairing_encryption,
        )?;
        self.notice = format!(
            "Saved host, port, and pairing encryption to {}/config.toml.",
            self.config.data_dir.display()
        );
        self.last_error = None;
        Ok(())
    }

    fn cycle_pairing_encryption(&mut self) {
        self.config.pairing_encryption = self.config.pairing_encryption.next();
        self.notice = format!(
            "Pairing encryption updated to {}. Save config to persist it.",
            pairing_encryption_label(self.config.pairing_encryption)
        );
        self.last_error = None;
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
        if self.view == TuiView::Observer {
            self.render_observer(frame);
            if self.pairing_qr.is_some() {
                let popup = self.pairing_qr_popup(frame.area());
                frame.render_widget(Clear, popup.area);
                frame.render_widget(
                    self.pairing_qr_paragraph(popup.lines, popup.title),
                    popup.area,
                );
            }
            return;
        }

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
        if self.pairing_qr.is_some() {
            let popup = self.pairing_qr_popup(frame.area());
            frame.render_widget(Clear, popup.area);
            frame.render_widget(
                self.pairing_qr_paragraph(popup.lines, popup.title),
                popup.area,
            );
        }
    }

    fn render_observer(&self, frame: &mut Frame<'_>) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(7),
                Constraint::Min(12),
                Constraint::Length(4),
            ])
            .split(frame.area());
        let main_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(42), Constraint::Min(48)])
            .split(chunks[1]);

        for area in [chunks[0], main_chunks[0], main_chunks[1], chunks[2]] {
            frame.render_widget(Clear, area);
        }

        let state = self.observer_state();
        frame.render_widget(self.observer_summary_panel(&state), chunks[0]);
        frame.render_widget(self.session_panel(&state), main_chunks[0]);
        frame.render_widget(
            self.session_detail_panel(&state, main_chunks[1]),
            main_chunks[1],
        );
        frame.render_widget(self.observer_help_panel(), chunks[2]);
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
            Line::from(format!(
                "Pairing encryption: {} (action e)",
                pairing_encryption_label(runtime_config.pairing_encryption)
            )),
            Line::from("Pairing QR: one-click link + auth token; app fetches protocol key"),
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
            "Edit pairing encryption",
            "Save settings",
            "Show pairing QR",
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
                    "Shortcuts: s start/stop, r restart, h host, p port, e encryption, w config, g QR, l logs, o observer, q quit.",
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

    fn pairing_qr_area(&self, area: Rect, content_width: u16, content_height: u16) -> Rect {
        let max_width = area
            .width
            .saturating_sub(QR_POPUP_MARGIN.saturating_mul(2))
            .max(1);
        let max_height = area
            .height
            .saturating_sub(QR_POPUP_MARGIN.saturating_mul(2))
            .max(1);
        let width = content_width.saturating_add(2).min(max_width).max(1);
        let height = content_height.saturating_add(2).min(max_height).max(1);
        Rect {
            x: area.x + (area.width.saturating_sub(width)) / 2,
            y: area.y + (area.height.saturating_sub(height)) / 2,
            width,
            height,
        }
    }

    fn pairing_qr_popup(&self, area: Rect) -> PairingQrPopup {
        let title = match self.pairing_qr.as_ref() {
            Some(qr) if qr.payloads.len() > 1 => format!(
                "Pairing QR {}/{} - Left/Right switch, Esc closes",
                qr.active_index + 1,
                qr.payloads.len()
            ),
            Some(_) => "Pairing QR - any key closes".to_owned(),
            None => "Pairing QR - any key closes".to_owned(),
        };
        let max_popup_width = area
            .width
            .saturating_sub(QR_POPUP_MARGIN.saturating_mul(2))
            .max(1);
        let max_popup_height = area
            .height
            .saturating_sub(QR_POPUP_MARGIN.saturating_mul(2))
            .max(1);
        let max_content_width = max_popup_width.saturating_sub(2).max(1);
        let max_content_height = max_popup_height.saturating_sub(2).max(1);
        let lines = self.pairing_qr_lines(max_content_width, max_content_height);
        let content_width = lines_width(&lines).min(max_content_width).max(1);
        let content_height = (lines.len() as u16).min(max_content_height).max(1);
        let area = self.pairing_qr_area(area, content_width, content_height);

        PairingQrPopup { area, lines, title }
    }

    fn pairing_qr_lines(&self, max_width: u16, max_height: u16) -> Vec<Line<'static>> {
        let Some(qr) = self.pairing_qr.as_ref() else {
            return vec![Line::from("Failed to render pairing QR.")];
        };
        let Some(payload) = qr.payloads.get(qr.active_index) else {
            return vec![Line::from("Failed to render pairing QR.")];
        };
        match render_qr_text_for_bounds(payload, max_width, max_height) {
            Ok(rendered) if rendered.width <= max_width && rendered.height <= max_height => {
                rendered
                    .text
                    .lines()
                    .map(|line| Line::from(line.to_owned()))
                    .collect()
            }
            Ok(rendered) => vec![
                Line::from("Terminal is too small for the pairing QR."),
                Line::from(format!(
                    "Need at least {}x{} cells for the compact code.",
                    rendered.width, rendered.height
                )),
                Line::from("Resize the window and it will redraw automatically."),
            ],
            Err(error) => vec![
                Line::from("Failed to render pairing QR."),
                Line::from(error.to_string()),
            ],
        }
    }

    fn pairing_qr_paragraph(&self, lines: Vec<Line<'static>>, title: String) -> Paragraph<'static> {
        Paragraph::new(lines)
            .style(Style::default().fg(Color::Black).bg(Color::White))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .style(Style::default().fg(Color::Black).bg(Color::White)),
            )
    }

    fn observer_state(&self) -> ObserverState {
        ObserverState::from_events(&self.live_events)
    }

    fn observer_summary_panel(&self, state: &ObserverState) -> Paragraph<'_> {
        let runtime_state = if self.server.is_some() {
            "running"
        } else {
            "stopped"
        };
        let selected = state
            .sessions
            .get(self.selected_session)
            .map(|session| session.session_id.as_str())
            .unwrap_or("-");

        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "Observer ",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::raw("read-only view for this TUI session"),
            ]),
            Line::from(format!(
                "Service: {runtime_state} | Sessions: {} | Active tasks: {} | Pending requests: {} | Events: {}",
                state.sessions.len(),
                state.active_task_count,
                state.pending_request_count,
                self.live_events.len()
            )),
            Line::from(format!("Selected session: {selected}")),
            Line::from("No service commands are available on this page."),
        ])
        .wrap(Wrap { trim: true })
        .block(Block::default().title("Session Observer").borders(Borders::ALL))
    }

    fn session_panel(&self, state: &ObserverState) -> List<'_> {
        let items = if state.sessions.is_empty() {
            vec![ListItem::new(Line::from("No Codex session events yet."))]
        } else {
            state
                .sessions
                .iter()
                .enumerate()
                .map(|(idx, session)| {
                    let marker = if idx == self.selected_session {
                        "> "
                    } else {
                        "  "
                    };
                    let style = if idx == self.selected_session {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    ListItem::new(Line::from(vec![
                        Span::raw(marker),
                        Span::raw(truncate_text(&session.session_id, 22)),
                        Span::raw(format!(
                            " {} ev {} turn {} req",
                            session.event_count,
                            session.turns.len(),
                            session.pending_request_count()
                        )),
                    ]))
                    .style(style)
                })
                .collect()
        };

        List::new(items).block(Block::default().title("Sessions").borders(Borders::ALL))
    }

    fn session_detail_panel(&self, state: &ObserverState, area: Rect) -> Paragraph<'_> {
        let lines = match state.sessions.get(self.selected_session) {
            Some(session) => session_detail_lines(session),
            None => vec![Line::from(
                "Start or connect a client to populate this observer.",
            )],
        };
        let content_height = area.height.saturating_sub(2) as usize;
        let max_scroll = lines.len().saturating_sub(content_height);
        let scroll = self.observer_scroll.min(max_scroll);
        let line_count = lines.len();

        Paragraph::new(lines)
            .scroll((scroll as u16, 0))
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .title(format!(
                        "History / Conversation / Current Tasks [{line_count}]"
                    ))
                    .borders(Borders::ALL),
            )
    }

    fn observer_help_panel(&self) -> Paragraph<'_> {
        Paragraph::new(vec![
            Line::from("Read-only navigation: Up/Down selects session, PageUp/PageDown scrolls details, Home/End jumps."),
            Line::from("q, Esc, o, or Tab returns to the control view. This page does not start, stop, approve, or send anything."),
        ])
        .wrap(Wrap { trim: true })
        .block(Block::default().title("Observer Controls").borders(Borders::ALL))
    }
}

#[derive(Debug, Default)]
struct ObserverState {
    sessions: Vec<ObservedSession>,
    active_task_count: usize,
    pending_request_count: usize,
}

impl ObserverState {
    fn from_events(events: &[EventRecord]) -> Self {
        let mut sessions = BTreeMap::<String, ObservedSession>::new();
        for event in events {
            let Some(session_id) = event_session_id(event) else {
                continue;
            };
            sessions
                .entry(session_id.clone())
                .or_insert_with(|| ObservedSession::new(session_id))
                .apply(event);
        }

        let mut sessions = sessions.into_values().collect::<Vec<_>>();
        sessions.sort_by(|left, right| {
            right
                .last_time
                .cmp(&left.last_time)
                .then_with(|| left.session_id.cmp(&right.session_id))
        });
        let active_task_count = sessions
            .iter()
            .map(ObservedSession::active_task_count)
            .sum();
        let pending_request_count = sessions
            .iter()
            .map(ObservedSession::pending_request_count)
            .sum();
        Self {
            sessions,
            active_task_count,
            pending_request_count,
        }
    }
}

#[derive(Debug)]
struct ObservedSession {
    session_id: String,
    event_count: usize,
    first_time: chrono::DateTime<Utc>,
    last_time: chrono::DateTime<Utc>,
    adapter_state: Option<String>,
    in_flight_command_id: Option<String>,
    child_pid: Option<String>,
    threads: BTreeSet<String>,
    turns: BTreeMap<String, ObservedTurn>,
    requests: BTreeMap<String, ObservedRequest>,
    plans: Vec<String>,
    cloud_tasks: BTreeMap<String, String>,
    conversation: Vec<String>,
    history: Vec<String>,
}

impl ObservedSession {
    fn new(session_id: String) -> Self {
        let now = Utc::now();
        Self {
            session_id,
            event_count: 0,
            first_time: now,
            last_time: now,
            adapter_state: None,
            in_flight_command_id: None,
            child_pid: None,
            threads: BTreeSet::new(),
            turns: BTreeMap::new(),
            requests: BTreeMap::new(),
            plans: Vec::new(),
            cloud_tasks: BTreeMap::new(),
            conversation: Vec::new(),
            history: Vec::new(),
        }
    }

    fn apply(&mut self, event: &EventRecord) {
        self.event_count += 1;
        if self.event_count == 1 {
            self.first_time = event.time;
        }
        self.last_time = event.time;

        let payload = event_payload_source(event);
        if let Some(thread_id) = event_thread_id(event) {
            self.threads.insert(thread_id);
        }
        if let Some(turn_id) = event_turn_id(event) {
            self.turns
                .entry(turn_id.clone())
                .or_insert_with(|| ObservedTurn::new(turn_id))
                .apply(event, payload);
        }
        if let Some(lifecycle_state) = payload_string_any(payload, &["lifecycleState"]) {
            self.adapter_state = Some(lifecycle_state);
        }
        if let Some(command_id) = payload
            .get("commandLane")
            .and_then(|lane| payload_string_any(lane, &["inFlightCommandId"]))
        {
            self.in_flight_command_id = Some(command_id);
        }
        if let Some(pid) = payload
            .get("childProcess")
            .and_then(|process| process.get("pid"))
            .map(compact_scalar)
        {
            self.child_pid = Some(pid);
        }
        if let Some(request_id) = payload_string_any(payload, &["requestId", "request_id", "id"]) {
            self.requests
                .entry(request_id.clone())
                .or_insert_with(|| ObservedRequest::new(request_id))
                .apply(event, payload);
        }
        if event.event_type == "codex.turn.planUpdated" || event.event_type == "codex.plan.delta" {
            if let Some(plan) = summarize_plan(payload) {
                self.plans.push(plan);
            }
        }
        if event.event_type.starts_with("codex.cloudTask.") {
            if let Some(task_id) = cloud_task_id(payload) {
                let status = payload_string_any(payload, &["status"])
                    .unwrap_or_else(|| event.event_type.clone());
                self.cloud_tasks.insert(task_id, status);
            }
        }
        if is_conversation_event(&event.event_type) {
            self.conversation.push(summarize_observer_event(event));
        }
        self.history.push(summarize_observer_event(event));
    }

    fn active_task_count(&self) -> usize {
        let active_turns = self
            .turns
            .values()
            .filter(|turn| turn.status.is_active())
            .count();
        let active_cloud_tasks = self
            .cloud_tasks
            .values()
            .filter(|status| !terminal_status(status))
            .count();
        active_turns + active_cloud_tasks + usize::from(self.in_flight_command_id.is_some())
    }

    fn pending_request_count(&self) -> usize {
        self.requests
            .values()
            .filter(|request| request.status == ObservedRequestStatus::Pending)
            .count()
    }
}

#[derive(Debug)]
struct ObservedTurn {
    turn_id: String,
    status: ObservedTurnStatus,
    last_event: String,
}

impl ObservedTurn {
    fn new(turn_id: String) -> Self {
        Self {
            turn_id,
            status: ObservedTurnStatus::Active,
            last_event: String::new(),
        }
    }

    fn apply(&mut self, event: &EventRecord, payload: &Value) {
        self.last_event = event.event_type.clone();
        self.status = match event.event_type.as_str() {
            "codex.turn.completed" => ObservedTurnStatus::Completed,
            "codex.turn.interrupted" => ObservedTurnStatus::Interrupted,
            "codex.turn.failed" | "codex.error" => ObservedTurnStatus::Failed,
            _ => payload_string_any(payload, &["status", "lifecycleState"])
                .map(|status| ObservedTurnStatus::from_status(&status))
                .unwrap_or(self.status),
        };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ObservedTurnStatus {
    Active,
    Completed,
    Interrupted,
    Failed,
}

impl ObservedTurnStatus {
    fn from_status(status: &str) -> Self {
        match status {
            "completed" | "ready" | "success" => Self::Completed,
            "interrupted" | "cancelled" | "canceled" => Self::Interrupted,
            "failed" | "error" => Self::Failed,
            _ => Self::Active,
        }
    }

    fn is_active(self) -> bool {
        self == Self::Active
    }
}

#[derive(Debug)]
struct ObservedRequest {
    request_id: String,
    request_type: String,
    status: ObservedRequestStatus,
}

impl ObservedRequest {
    fn new(request_id: String) -> Self {
        Self {
            request_id,
            request_type: "-".to_owned(),
            status: ObservedRequestStatus::Observed,
        }
    }

    fn apply(&mut self, event: &EventRecord, payload: &Value) {
        self.request_type = payload_string_any(payload, &["operation", "responseType"])
            .unwrap_or_else(|| event.event_type.clone());
        let outcome = payload_string_any(payload, &["outcome", "decision"]);
        self.status =
            if outcome.as_deref() == Some("pending") || event.event_type.ends_with(".request") {
                ObservedRequestStatus::Pending
            } else if event.event_type == "codex.serverRequest.resolved"
                || event.event_type.ends_with(".resolved")
                || outcome.is_some()
            {
                ObservedRequestStatus::Resolved
            } else {
                ObservedRequestStatus::Observed
            };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ObservedRequestStatus {
    Observed,
    Pending,
    Resolved,
}

impl ObservedRequestStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Observed => "observed",
            Self::Pending => "pending",
            Self::Resolved => "resolved",
        }
    }
}

fn session_detail_lines(session: &ObservedSession) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("Session: ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(session.session_id.clone()),
    ]));
    lines.push(Line::from(format!(
        "First event: {} | Last event: {} | Events: {}",
        session.first_time.format("%H:%M:%S"),
        session.last_time.format("%H:%M:%S"),
        session.event_count
    )));
    lines.push(Line::from(format!(
        "Adapter: {} | In-flight: {} | Child PID: {}",
        session.adapter_state.as_deref().unwrap_or("-"),
        session.in_flight_command_id.as_deref().unwrap_or("-"),
        session.child_pid.as_deref().unwrap_or("-")
    )));
    lines.push(Line::from(format!(
        "Threads: {} | Turns: {} | Pending requests: {} | Cloud tasks: {}",
        session.threads.len(),
        session.turns.len(),
        session.pending_request_count(),
        session.cloud_tasks.len()
    )));

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Current Running Tasks",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    let mut active_lines = Vec::new();
    if let Some(command_id) = &session.in_flight_command_id {
        active_lines.push(format!("command in flight: {command_id}"));
    }
    active_lines.extend(
        session
            .turns
            .values()
            .filter(|turn| turn.status.is_active())
            .map(|turn| format!("turn {} ({})", turn.turn_id, turn.last_event)),
    );
    active_lines.extend(
        session
            .requests
            .values()
            .filter(|request| request.status == ObservedRequestStatus::Pending)
            .map(|request| {
                format!(
                    "pending request {} ({})",
                    request.request_id, request.request_type
                )
            }),
    );
    active_lines.extend(
        session
            .cloud_tasks
            .iter()
            .filter(|(_, status)| !terminal_status(status))
            .map(|(task_id, status)| format!("cloud task {task_id} ({status})")),
    );
    if active_lines.is_empty() {
        lines.push(Line::from("No active task inferred from current events."));
    } else {
        lines.extend(active_lines.into_iter().map(Line::from));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Conversation Records",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    if session.conversation.is_empty() {
        lines.push(Line::from("No conversation item events observed yet."));
    } else {
        lines.extend(
            session
                .conversation
                .iter()
                .map(|line| Line::from(line.clone())),
        );
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Plans",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    if session.plans.is_empty() {
        lines.push(Line::from("No plan updates observed yet."));
    } else {
        lines.extend(
            session
                .plans
                .iter()
                .rev()
                .take(12)
                .map(|line| Line::from(line.clone())),
        );
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Requests",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    if session.requests.is_empty() {
        lines.push(Line::from("No request records observed yet."));
    } else {
        lines.extend(session.requests.values().map(|request| {
            Line::from(format!(
                "{} [{}] {}",
                request.request_id,
                request.status.as_str(),
                request.request_type
            ))
        }));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Full Event History",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    lines.extend(session.history.iter().map(|line| Line::from(line.clone())));
    lines
}

fn event_session_id(event: &EventRecord) -> Option<String> {
    payload_string_any(
        &event.payload,
        &[
            "codex_session_id",
            "codexSessionId",
            "sessionId",
            "session_id",
        ],
    )
    .or_else(|| {
        event.payload.get("data").and_then(|data| {
            payload_string_any(
                data,
                &[
                    "codex_session_id",
                    "codexSessionId",
                    "sessionId",
                    "session_id",
                ],
            )
        })
    })
}

fn event_thread_id(event: &EventRecord) -> Option<String> {
    payload_string_any(
        &event.payload,
        &["codex_thread_id", "codexThreadId", "threadId", "thread_id"],
    )
    .or_else(|| {
        event.payload.get("data").and_then(|data| {
            payload_string_any(
                data,
                &["codex_thread_id", "codexThreadId", "threadId", "thread_id"],
            )
        })
    })
}

fn event_turn_id(event: &EventRecord) -> Option<String> {
    payload_string_any(
        &event.payload,
        &["codex_turn_id", "codexTurnId", "turnId", "turn_id"],
    )
    .or_else(|| {
        event.payload.get("data").and_then(|data| {
            payload_string_any(data, &["codex_turn_id", "codexTurnId", "turnId", "turn_id"])
        })
    })
}

fn event_payload_source(event: &EventRecord) -> &Value {
    event.payload.get("data").unwrap_or(&event.payload)
}

fn payload_string_any(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    })
}

fn summarize_observer_event(event: &EventRecord) -> String {
    let cursor = event
        .payload
        .get("cursor")
        .map(compact_scalar)
        .map(|cursor| format!("#{cursor} "))
        .unwrap_or_default();
    format!("{}{}", cursor, summarize_event(event))
}

fn summarize_plan(payload: &Value) -> Option<String> {
    if let Some(plan) = payload
        .get("plan")
        .or_else(|| payload.get("items"))
        .and_then(Value::as_array)
    {
        let summary = plan
            .iter()
            .filter_map(|item| {
                let text = payload_string_any(item, &["step", "text"])?;
                let status =
                    payload_string_any(item, &["status"]).unwrap_or_else(|| "pending".to_owned());
                Some(format!("{status}: {}", truncate_text(&text, 56)))
            })
            .collect::<Vec<_>>()
            .join(" | ");
        return Some(truncate_text(&summary, 180));
    }
    payload_string_any(payload, &["delta", "explanation"]).map(|text| truncate_text(&text, 180))
}

fn cloud_task_id(payload: &Value) -> Option<String> {
    payload_string_any(payload, &["taskId", "task_id", "id"]).or_else(|| {
        payload
            .get("task")
            .and_then(|task| payload_string_any(task, &["taskId", "task_id", "id"]))
    })
}

fn terminal_status(status: &str) -> bool {
    matches!(
        status,
        "completed"
            | "complete"
            | "succeeded"
            | "success"
            | "failed"
            | "error"
            | "interrupted"
            | "cancelled"
            | "canceled"
            | "ready"
            | "stopped"
    )
}

fn is_conversation_event(event_type: &str) -> bool {
    event_type.starts_with("codex.item.")
        || matches!(
            event_type,
            "codex.turn.started"
                | "codex.turn.completed"
                | "codex.turn.failed"
                | "codex.turn.interrupted"
                | "codex.turn.planUpdated"
        )
}

fn lines_width(lines: &[Line<'_>]) -> u16 {
    lines
        .iter()
        .map(|line| line.width() as u16)
        .max()
        .unwrap_or(0)
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

fn pairing_encryption_label(value: PairingEncryption) -> &'static str {
    match value {
        PairingEncryption::None => "无",
        PairingEncryption::MlKem768 => "后量子",
        PairingEncryption::X25519 => "X25519",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{validate_host, validate_port, ObservedRequestStatus, ObserverState};
    use crate::event::EventRecord;

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

    #[test]
    fn observer_state_groups_session_history_and_active_requests() {
        let events = vec![
            EventRecord::new(
                "codex.control.ready",
                None,
                None,
                None,
                json!({
                    "cursor": 1,
                    "codex_session_id": "session-1",
                    "data": {
                        "codexSessionId": "session-1",
                        "lifecycleState": "ready",
                        "childProcess": { "pid": 42 }
                    }
                }),
            ),
            EventRecord::new(
                "codex.turn.started",
                None,
                None,
                None,
                json!({
                    "cursor": 2,
                    "codex_session_id": "session-1",
                    "codex_turn_id": "turn-1",
                    "data": {
                        "threadId": "thread-1",
                        "turnId": "turn-1"
                    }
                }),
            ),
            EventRecord::new(
                "codex.approval.commandExecution.request",
                None,
                None,
                None,
                json!({
                    "cursor": 3,
                    "codex_session_id": "session-1",
                    "data": {
                        "requestId": "approval-1",
                        "outcome": "pending"
                    }
                }),
            ),
        ];

        let state = ObserverState::from_events(&events);
        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.active_task_count, 1);
        assert_eq!(state.pending_request_count, 1);

        let session = &state.sessions[0];
        assert_eq!(session.session_id, "session-1");
        assert_eq!(session.event_count, 3);
        assert_eq!(session.adapter_state.as_deref(), Some("ready"));
        assert_eq!(session.child_pid.as_deref(), Some("42"));
        assert!(session.threads.contains("thread-1"));
        assert_eq!(
            session.requests["approval-1"].status,
            ObservedRequestStatus::Pending
        );
        assert_eq!(session.conversation.len(), 1);
    }
}
