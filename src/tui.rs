use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use arboard::Clipboard;
use chrono::Utc;
use crossterm::event::{self, Event, KeyCode, KeyEvent};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use serde_json::Value;

use crate::config::{Config, PairingEncryption, ServeArgs};
use crate::daemon::{self, DaemonProcess};
use crate::event::EventRecord;
use crate::transport_crypto::{render_qr_text_for_bounds, PairingKeys};
use crate::workspace_paths::canonical_workspace_root;

const ACTION_COUNT: usize = 12;
const MAX_LOG_LINES: usize = 256;
const LOG_SCROLL_STEP: usize = 6;
const QR_POPUP_MARGIN: u16 = 1;
const EDIT_POPUP_WIDTH: u16 = 64;

mod positioned_backend;
use positioned_backend::PositionedBackend;

// Box-drawing glyphs have ambiguous terminal widths. ASCII frame characters
// stay one cell wide even when the host uses a different Unicode width table.
fn panel_block<'a>() -> Block<'a> {
    Block::default().border_set(ratatui::symbols::border::Set {
        top_left: "+",
        top_right: "+",
        bottom_left: "+",
        bottom_right: "+",
        vertical_left: "|",
        vertical_right: "|",
        horizontal_top: "-",
        horizontal_bottom: "-",
    })
}

pub async fn run(args: ServeArgs) -> Result<()> {
    let config = Config::load(args).context("failed to load configuration")?;
    let mut terminal = init_terminal()?;
    let _guard = TerminalGuard;
    let mut app = TuiApp::new(config);

    loop {
        app.refresh_daemon_status();
        app.refresh_device_pairing(false);
        terminal.draw(|frame| app.render(frame))?;

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) => {
                    if app.handle_key(key).await? {
                        break;
                    }
                }
                _ => {}
            }
        }
    }

    let exit_notice = app
        .text(
            "TUI exiting; exporting logs automatically.",
            "TUI 正在退出并自动导出日志。",
        )
        .to_owned();
    app.push_log(exit_notice);
    app.save_logs().context("failed to auto-save TUI logs")?;

    Ok(())
}

fn init_terminal() -> Result<Terminal<PositionedBackend<io::BufWriter<io::Stdout>>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    // Absolute cell positioning emits more ANSI bytes. Buffer them so cursor
    // corrections are delivered together instead of making per-cell writes.
    match Terminal::new(PositionedBackend::new(io::BufWriter::new(stdout))) {
        Ok(mut terminal) => {
            terminal.clear()?;
            Ok(terminal)
        }
        Err(error) => {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
            Err(error).context("failed to initialize terminal")
        }
    }
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
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

struct CredentialsPopup {
    auth_token: Option<String>,
    public_key: Option<String>,
    selected: usize,
    scroll: u16,
}

#[derive(Clone)]
struct DevicePairingRequest {
    request_id: String,
    verification_code: String,
    device_name: String,
    expires_at: i64,
}

#[derive(Default)]
struct DevicePairingState {
    open: bool,
    requests: Vec<DevicePairingRequest>,
    selected_id: Option<String>,
    displayed_request: RefCell<Option<(String, String)>>,
    refreshed_at: Option<Instant>,
    error: Option<String>,
}

impl DevicePairingState {
    fn replace(&mut self, requests: Vec<DevicePairingRequest>) {
        // Never approve a different request merely because a list index moved.
        if self
            .selected_id
            .as_ref()
            .is_some_and(|id| !requests.iter().any(|request| &request.request_id == id))
        {
            self.selected_id = None;
        }
        self.requests = requests;
    }

    fn navigate(&mut self, down: bool) {
        let current = self.selected_id.as_ref().and_then(|id| {
            self.requests
                .iter()
                .position(|request| &request.request_id == id)
        });
        let index = match current {
            Some(index) if down => (index + 1).min(self.requests.len().saturating_sub(1)),
            Some(index) => index.saturating_sub(1),
            None => 0,
        };
        self.selected_id = self
            .requests
            .get(index)
            .map(|request| request.request_id.clone());
    }

    fn selected(&self) -> Option<&DevicePairingRequest> {
        self.selected_id.as_ref().and_then(|id| {
            self.requests
                .iter()
                .find(|request| &request.request_id == id)
        })
    }
}

fn device_display_name(value: &str) -> String {
    value
        .chars()
        .filter(|character| {
            !character.is_control()
                && !matches!(*character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(100)
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TuiLanguage {
    English,
    Chinese,
}

impl TuiLanguage {
    fn detect() -> Self {
        let locale = std::env::var("LC_ALL")
            .or_else(|_| std::env::var("LC_MESSAGES"))
            .or_else(|_| std::env::var("LANG"))
            .unwrap_or_default()
            .to_ascii_lowercase();
        if locale.starts_with("zh") {
            Self::Chinese
        } else {
            Self::English
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "zh" | "zh-cn" | "zh-tw" | "chinese" => Some(Self::Chinese),
            "en" | "en-us" | "english" => Some(Self::English),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::Chinese => "zh-CN",
        }
    }

    fn text<'a>(self, english: &'a str, chinese: &'a str) -> &'a str {
        match self {
            Self::English => english,
            Self::Chinese => chinese,
        }
    }
}

struct TuiApp {
    config: Config,
    daemon: Option<DaemonProcess>,
    view: TuiView,
    selected_action: usize,
    selected_session: usize,
    edit: Option<EditMode>,
    last_error: Option<String>,
    notice: String,
    live_logs: VecDeque<String>,
    live_events: Vec<EventRecord>,
    log_scroll: usize,
    observer_scroll: usize,
    log_follow_tail: bool,
    pairing_qr: Option<PairingQr>,
    pairing_browser_pages: Vec<crate::transport_crypto::pairing_browser::PairingQrBrowserPage>,
    credentials: Option<CredentialsPopup>,
    device_pairing: DevicePairingState,
    folder_picker: Option<FolderPicker>,
    language: TuiLanguage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TuiView {
    Control,
    Observer,
}

impl TuiApp {
    fn new(config: Config) -> Self {
        let language = Config::load_tui_language(&config.data_dir)
            .ok()
            .flatten()
            .and_then(|value| TuiLanguage::parse(&value))
            .unwrap_or_else(TuiLanguage::detect);
        Self {
            config,
            daemon: None,
            view: TuiView::Control,
            selected_action: 0,
            selected_session: 0,
            edit: None,
            last_error: None,
            notice: language
                .text(
                    "Daemon is stopped. Select Start when ready.",
                    "Daemon 已停止，请选择启动。",
                )
                .to_owned(),
            live_logs: VecDeque::new(),
            live_events: Vec::new(),
            log_scroll: 0,
            observer_scroll: 0,
            log_follow_tail: true,
            pairing_qr: None,
            pairing_browser_pages: Vec::new(),
            credentials: None,
            device_pairing: DevicePairingState::default(),
            folder_picker: None,
            language,
        }
    }

    fn text<'a>(&self, english: &'a str, chinese: &'a str) -> &'a str {
        self.language.text(english, chinese)
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

    async fn show_pairing_qr(&mut self) {
        let Some(process) = self.daemon.as_ref() else {
            self.notice = self
                .text(
                    "Start the daemon before showing a pairing QR.",
                    "请先启动 daemon，再显示配对二维码。",
                )
                .to_owned();
            return;
        };

        let mut config = self.config.clone();
        config.host = process.host.clone();
        config.port = process.port;
        let qr = daemon::pairing_qr_payloads(&config, process.port).await;
        match qr {
            Ok(payloads) => {
                let total = payloads.len();
                self.pairing_qr = Some(PairingQr {
                    payloads,
                    active_index: 0,
                });
                self.notice = match (self.language, total > 1) {
                    (TuiLanguage::English, true) => format!("Pairing QR is open in the center window. Use Left/Right to switch {total} segments; b opens a browser."),
                    (TuiLanguage::Chinese, true) => format!("配对二维码已打开。使用左右方向键切换 {total} 个分段，按 b 在浏览器查看。"),
                    (TuiLanguage::English, false) => "Pairing QR is open in the center window; b opens a browser.".to_owned(),
                    (TuiLanguage::Chinese, false) => "配对二维码已在中央窗口打开，按 b 在浏览器查看。".to_owned(),
                };
            }
            Err(error) => {
                self.notice = self
                    .text("Failed to render pairing QR.", "无法生成配对二维码。")
                    .to_owned();
                self.last_error = Some(error.to_string());
                return;
            }
        };
    }

    async fn show_credentials(&mut self) {
        match PairingKeys::load_or_generate(&self.config.data_dir).await {
            Ok(keys) => {
                self.credentials = Some(CredentialsPopup {
                    auth_token: self.config.security.auth_token.clone(),
                    public_key: keys.pairing_public_key(self.config.pairing_encryption),
                    selected: 0,
                    scroll: 0,
                });
                self.notice = self
                    .text(
                        "Credentials are open. Select an item and press Enter or c to copy.",
                        "凭据窗口已打开。选择项目后按 Enter 或 c 复制。",
                    )
                    .to_owned();
            }
            Err(error) => {
                self.notice = self
                    .text("Failed to load pairing keys.", "无法读取配对密钥。")
                    .to_owned();
                self.last_error = Some(error.to_string());
            }
        }
    }

    fn close_credentials(&mut self) {
        self.credentials = None;
        self.notice = self
            .text("Credentials closed.", "凭据窗口已关闭。")
            .to_owned();
    }

    fn copy_selected_credential(&mut self) {
        let Some(credentials) = self.credentials.as_ref() else {
            return;
        };
        let (label, value) = if credentials.selected == 0 {
            (
                self.text("Auth Token", "认证令牌").to_owned(),
                credentials.auth_token.clone(),
            )
        } else {
            (
                self.text("Encryption public key", "加密公钥").to_owned(),
                credentials.public_key.clone(),
            )
        };
        let Some(value) = value.filter(|value| !value.is_empty()) else {
            self.notice = self
                .text(
                    "The selected credential is not available.",
                    "所选凭据不可用。",
                )
                .to_owned();
            return;
        };
        match Clipboard::new().and_then(|mut clipboard| clipboard.set_text(value)) {
            Ok(()) => {
                self.notice = match self.language {
                    TuiLanguage::English => format!("{label} copied to the system clipboard."),
                    TuiLanguage::Chinese => format!("{label}已复制到系统剪贴板。"),
                };
                self.last_error = None;
            }
            Err(error) => {
                self.notice = self
                    .text("Clipboard copy failed.", "复制到剪贴板失败。")
                    .to_owned();
                self.last_error = Some(error.to_string());
            }
        }
    }

    fn toggle_language(&mut self) {
        self.language = match self.language {
            TuiLanguage::English => TuiLanguage::Chinese,
            TuiLanguage::Chinese => TuiLanguage::English,
        };
        match Config::save_tui_language(self.config.data_dir.clone(), self.language.as_str()) {
            Ok(()) => {
                self.notice = self
                    .text(
                        "Language changed to English and saved.",
                        "界面语言已切换为中文并保存。",
                    )
                    .to_owned();
                self.last_error = None;
            }
            Err(error) => {
                self.notice = self
                    .text(
                        "Language changed, but saving the preference failed.",
                        "界面语言已切换，但保存语言偏好失败。",
                    )
                    .to_owned();
                self.last_error = Some(error.to_string());
            }
        }
    }

    async fn open_pairing_qr_browser(&mut self) {
        let Some(qr) = self.pairing_qr.as_ref() else {
            return;
        };
        match crate::transport_crypto::pairing_browser::open_pairing_qr_browser(&qr.payloads).await
        {
            Ok(page) => {
                self.pairing_browser_pages.push(page);
                self.notice = self
                    .text(
                        "Pairing QR opened in your browser.",
                        "已在浏览器中打开配对二维码。",
                    )
                    .to_owned();
                self.last_error = None;
            }
            Err(error) => {
                self.notice = self
                    .text(
                        "Could not open the pairing QR in a browser.",
                        "无法在浏览器中打开配对二维码。",
                    )
                    .to_owned();
                self.last_error = Some(error.to_string());
            }
        }
    }

    fn close_pairing_qr(&mut self) {
        if self.pairing_qr.is_some() {
            self.pairing_qr = None;
            self.notice = self
                .text("Pairing QR closed.", "配对二维码已关闭。")
                .to_owned();
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

    fn refresh_device_pairing(&mut self, force: bool) {
        if !force
            && self
                .device_pairing
                .refreshed_at
                .is_some_and(|time| time.elapsed() < Duration::from_millis(500))
        {
            return;
        }
        self.device_pairing.refreshed_at = Some(Instant::now());
        let data_dir = self
            .daemon
            .as_ref()
            .map(|process| process.data_dir.as_path())
            .unwrap_or(self.config.data_dir.as_path());
        match crate::device_pairing::list_device_pairing_requests(data_dir) {
            Ok(requests) => {
                self.device_pairing.replace(
                    requests
                        .into_iter()
                        .map(|request| DevicePairingRequest {
                            request_id: request.request_id,
                            verification_code: request.verification_code,
                            device_name: request.device_name,
                            expires_at: i64::try_from(request.expires_at).unwrap_or(i64::MAX),
                        })
                        .collect(),
                );
                self.device_pairing.error = None;
            }
            Err(_) => {
                self.device_pairing.replace(Vec::new());
                self.device_pairing.error = Some(
                    self.text(
                        "Cannot read local device requests.",
                        "无法读取本机设备验证请求。",
                    )
                    .to_owned(),
                );
            }
        }
    }

    fn decide_device_pairing(&mut self, approved: bool) {
        // Approval is only valid for the exact request/code actually rendered.
        // A tiny terminal or a changed selection must not hide what is approved.
        let displayed = self.device_pairing.displayed_request.borrow().clone();
        if approved
            && !self.device_pairing.selected().is_some_and(|request| {
                displayed.as_ref()
                    == Some(&(
                        request.request_id.clone(),
                        request.verification_code.clone(),
                    ))
            })
        {
            self.device_pairing.error = Some(
                self.text(
                    "Enlarge the terminal to view and verify the full code before approving.",
                    "请放大终端，查看并核对完整验证码后再批准。",
                )
                .to_owned(),
            );
            return;
        }
        // Recheck TTL and pending state, preserving request identity through refresh.
        self.refresh_device_pairing(true);
        let Some(request) = self.device_pairing.selected() else {
            return;
        };
        if request.expires_at <= Utc::now().timestamp_millis() {
            return;
        }
        if approved
            && displayed.as_ref()
                != Some(&(
                    request.request_id.clone(),
                    request.verification_code.clone(),
                ))
        {
            return;
        }
        let request_id = request.request_id.clone();
        let data_dir = self
            .daemon
            .as_ref()
            .map(|process| process.data_dir.as_path())
            .unwrap_or(self.config.data_dir.as_path());
        match crate::device_pairing::decide_device_pairing(data_dir, &request_id, approved) {
            Ok(()) => {
                self.device_pairing.selected_id = None;
                self.refresh_device_pairing(true);
                self.notice = if approved {
                    self.text(
                        "Device approval recorded locally.",
                        "已在本机记录设备批准。",
                    )
                    .to_owned()
                } else {
                    self.text(
                        "Device rejection recorded locally.",
                        "已在本机记录设备拒绝。",
                    )
                    .to_owned()
                };
            }
            Err(_) => {
                self.device_pairing.error = Some(
                    self.text(
                        "Could not record the decision; check whether the request expired.",
                        "无法记录决定，请检查请求是否已过期。",
                    )
                    .to_owned(),
                )
            }
        }
    }

    fn open_device_pairing(&mut self) {
        self.refresh_device_pairing(true);
        self.device_pairing.open = true;
        self.device_pairing.selected_id = self
            .device_pairing
            .requests
            .first()
            .map(|request| request.request_id.clone());
    }

    fn handle_device_pairing_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.device_pairing.open = false,
            KeyCode::Up | KeyCode::Char('k') => self.device_pairing.navigate(false),
            KeyCode::Down | KeyCode::Char('j') => self.device_pairing.navigate(true),
            KeyCode::Char('a') | KeyCode::Char('r')
                if key.kind == crossterm::event::KeyEventKind::Press =>
            {
                self.decide_device_pairing(key.code == KeyCode::Char('a'));
            }
            // Enter is intentionally inert: approval requires the explicit a key.
            _ => {}
        }
    }

    fn render_device_pairing(&self, frame: &mut Frame<'_>) {
        self.device_pairing.displayed_request.replace(None);
        let area = centered_area(frame.area(), 88, 24);
        frame.render_widget(Clear, area);
        let block = panel_block()
            .borders(Borders::ALL)
            .title(self.text("Device verification", "设备验证"));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width < 36 || inner.height < 18 {
            frame.render_widget(
                Paragraph::new(self.text(
                    "Enlarge the terminal to verify the code. Approval is disabled. Esc closes.",
                    "请放大终端查看验证码。当前禁止批准，Esc 关闭。",
                ))
                .wrap(Wrap { trim: true }),
                inner,
            );
            return;
        }
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(3),
                Constraint::Length(10),
                Constraint::Length(2),
            ])
            .split(inner);
        frame.render_widget(Paragraph::new(self.text(
            "Compare the full code with your client before approving. Device names are self-reported.",
            "批准前请核对客户端的完整验证码。设备名称由请求方自行填写。",
        )).wrap(Wrap { trim: true }), sections[0]);
        let selected_index = self
            .device_pairing
            .selected_id
            .as_ref()
            .and_then(|id| {
                self.device_pairing
                    .requests
                    .iter()
                    .position(|request| &request.request_id == id)
            })
            .unwrap_or(0);
        let offset = selected_index.saturating_sub(sections[1].height.saturating_sub(1) as usize);
        let lines: Vec<Line<'static>> = if self.device_pairing.requests.is_empty() {
            vec![Line::from(
                self.text("No pending devices.", "暂无待验证设备。")
                    .to_owned(),
            )]
        } else {
            self.device_pairing
                .requests
                .iter()
                .skip(offset)
                .map(|request| {
                    let selected = self.device_pairing.selected_id.as_deref()
                        == Some(request.request_id.as_str());
                    Line::styled(
                        format!(
                            "{}{}",
                            if selected { "> " } else { "  " },
                            device_display_name(&request.device_name)
                        ),
                        if selected {
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        },
                    )
                })
                .collect()
        };
        frame.render_widget(Paragraph::new(lines), sections[1]);
        let details = if let Some(request) = self.device_pairing.selected() {
            self.device_pairing.displayed_request.replace(Some((
                request.request_id.clone(),
                request.verification_code.clone(),
            )));
            let remaining = request
                .expires_at
                .saturating_sub(Utc::now().timestamp_millis())
                .max(0)
                / 1000;
            vec![
                Line::from(self.text("Verification code:", "验证码：").to_owned()),
                Line::styled(
                    request.verification_code.clone(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Line::from(format!(
                    "{}{}",
                    self.text("Device (self-reported): ", "设备（自报）："),
                    device_display_name(&request.device_name)
                )),
                Line::from(format!(
                    "{}{}",
                    self.text("Request: ", "请求："),
                    device_display_name(&request.request_id)
                )),
                Line::from(match self.language {
                    TuiLanguage::English => format!("Expires in {remaining}s"),
                    TuiLanguage::Chinese => format!("{remaining} 秒后过期"),
                }),
            ]
        } else {
            vec![Line::from(
                self.text(
                    "Use Up/Down to select a pending request.",
                    "请使用上下键选择待验证请求。",
                )
                .to_owned(),
            )]
        };
        frame.render_widget(
            Paragraph::new(details).wrap(Wrap { trim: true }),
            sections[2],
        );
        let footer = self.device_pairing.error.clone().unwrap_or_else(|| {
            self.text(
                "a approve · r reject · Up/Down select · Esc close (Enter does not approve)",
                "a 批准 · r 拒绝 · 上下选择 · Esc 关闭（Enter 不会批准）",
            )
            .to_owned()
        });
        frame.render_widget(
            Paragraph::new(footer).wrap(Wrap { trim: true }),
            sections[3],
        );
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if self.device_pairing.open {
            self.handle_device_pairing_key(key);
            return Ok(false);
        }
        if self.credentials.is_some() {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.close_credentials(),
                KeyCode::Up | KeyCode::Char('k') => {
                    if let Some(credentials) = &mut self.credentials {
                        credentials.selected = credentials.selected.saturating_sub(1);
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if let Some(credentials) = &mut self.credentials {
                        credentials.selected = (credentials.selected + 1).min(1);
                    }
                }
                KeyCode::PageUp => {
                    if let Some(credentials) = &mut self.credentials {
                        credentials.scroll =
                            credentials.scroll.saturating_sub(LOG_SCROLL_STEP as u16);
                    }
                }
                KeyCode::PageDown => {
                    if let Some(credentials) = &mut self.credentials {
                        credentials.scroll =
                            credentials.scroll.saturating_add(LOG_SCROLL_STEP as u16);
                    }
                }
                KeyCode::Home => {
                    if let Some(credentials) = &mut self.credentials {
                        credentials.scroll = 0;
                    }
                }
                KeyCode::Enter | KeyCode::Char('c') => self.copy_selected_credential(),
                _ => {}
            }
            return Ok(false);
        }

        if self.pairing_qr.is_some() {
            match key.code {
                KeyCode::Char('b') => self.open_pairing_qr_browser().await,
                KeyCode::Left | KeyCode::PageUp => self.previous_pairing_qr(),
                KeyCode::Right | KeyCode::PageDown => self.next_pairing_qr(),
                KeyCode::Esc | KeyCode::Char('q') => self.close_pairing_qr(),
                _ => self.close_pairing_qr(),
            }
            return Ok(false);
        }

        if self.folder_picker.is_some() {
            self.handle_folder_picker_key(key).await?;
            return Ok(false);
        }

        if self.edit.is_some() {
            self.handle_edit_key(key).await?;
            return Ok(false);
        }

        if self.view == TuiView::Observer {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('o') | KeyCode::Tab => {
                    self.view = TuiView::Control;
                    self.notice = self
                        .text("Returned to the control view.", "已返回控制视图。")
                        .to_owned();
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
                self.notice = self
                    .text(
                        "Observer is read-only. Use q, Esc, o, or Tab to return.",
                        "观察器为只读视图。按 q、Esc、o 或 Tab 返回。",
                    )
                    .to_owned();
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
            KeyCode::Char('s') => self.toggle_daemon().await?,
            KeyCode::Char('r') => self.restart_daemon().await?,
            KeyCode::Char('h') => self.start_host_edit(),
            KeyCode::Char('p') => self.start_port_edit(),
            KeyCode::Char('w') => self.start_workspace_root_picker(),
            KeyCode::Char('e') => self.start_pairing_encryption_edit(),
            KeyCode::Char('x') => self.start_reset_edit(),
            KeyCode::Char('g') => self.show_pairing_qr().await,
            KeyCode::Char('c') => self.show_credentials().await,
            KeyCode::Char('d') => self.open_device_pairing(),
            KeyCode::Char('l') => self.toggle_language(),
            KeyCode::PageUp => self.scroll_logs_up(LOG_SCROLL_STEP),
            KeyCode::PageDown => self.scroll_logs_down(LOG_SCROLL_STEP),
            KeyCode::Home => self.scroll_logs_to_top(),
            KeyCode::End => self.scroll_logs_to_bottom(),
            _ => {}
        }

        Ok(false)
    }

    async fn handle_folder_picker_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.folder_picker = None;
                self.notice = self
                    .text(
                        "Workspace root selection canceled.",
                        "已取消选择工作区根目录。",
                    )
                    .to_owned();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(picker) = &mut self.folder_picker {
                    picker.selected = picker.selected.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(picker) = &mut self.folder_picker {
                    picker.select_next();
                }
            }
            KeyCode::PageUp => {
                if let Some(picker) = &mut self.folder_picker {
                    picker.selected = picker.selected.saturating_sub(LOG_SCROLL_STEP);
                }
            }
            KeyCode::PageDown => {
                if let Some(picker) = &mut self.folder_picker {
                    picker.select_next_by(LOG_SCROLL_STEP);
                }
            }
            KeyCode::Left | KeyCode::Backspace => {
                if let Some(picker) = &mut self.folder_picker {
                    picker.open_parent();
                }
            }
            KeyCode::Right | KeyCode::Enter => {
                if let Some(picker) = &mut self.folder_picker {
                    picker.open_selected();
                }
            }
            KeyCode::Home => {
                if let Some(picker) = &mut self.folder_picker {
                    picker.open_home();
                }
            }
            KeyCode::Char('r') => {
                if let Some(picker) = &mut self.folder_picker {
                    picker.refresh();
                }
            }
            KeyCode::Char(' ') => self.apply_selected_workspace_root().await?,
            _ => {}
        }

        Ok(())
    }

    async fn handle_edit_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => {
                self.edit = None;
                self.notice = self.text("Edit canceled.", "已取消编辑。").to_owned();
            }
            KeyCode::Enter => self.commit_edit().await?,
            KeyCode::Backspace => {
                if let Some(EditMode::Text { value, .. }) = &mut self.edit {
                    value.pop();
                }
            }
            KeyCode::Char(ch) => {
                if let Some(EditMode::Text { value, .. }) = &mut self.edit {
                    value.push(ch);
                } else if let Some(EditMode::Encryption { value }) = &mut self.edit {
                    if ch == 'e' {
                        *value = (*value).next();
                    }
                } else if let Some(EditMode::Reset { target }) = &mut self.edit {
                    if ch == 'x' {
                        *target = next_reset_target(*target);
                    }
                }
            }
            KeyCode::Left | KeyCode::PageUp => match &mut self.edit {
                Some(EditMode::Encryption { value }) => {
                    *value = previous_pairing_encryption(*value);
                }
                Some(EditMode::Reset { target }) => {
                    *target = previous_reset_target(*target);
                }
                _ => {}
            },
            KeyCode::Right | KeyCode::PageDown => match &mut self.edit {
                Some(EditMode::Encryption { value }) => {
                    *value = (*value).next();
                }
                Some(EditMode::Reset { target }) => {
                    *target = next_reset_target(*target);
                }
                _ => {}
            },
            _ => {}
        }

        Ok(())
    }

    async fn run_selected_action(&mut self) -> Result<bool> {
        match self.selected_action {
            0 => self.toggle_daemon().await?,
            1 => self.restart_daemon().await?,
            2 => self.start_host_edit(),
            3 => self.start_port_edit(),
            4 => self.start_workspace_root_picker(),
            5 => self.start_pairing_encryption_edit(),
            6 => self.start_reset_edit(),
            7 => self.show_pairing_qr().await,
            8 => self.show_credentials().await,
            9 => self.toggle_language(),
            10 => self.open_device_pairing(),
            11 => return Ok(true),
            _ => {}
        }
        Ok(false)
    }

    async fn toggle_daemon(&mut self) -> Result<()> {
        if self.daemon.is_some() {
            self.stop_daemon().await
        } else {
            self.start_daemon().await
        }
    }

    async fn start_daemon(&mut self) -> Result<()> {
        if self.daemon.is_some() {
            self.notice = self
                .text("Daemon is already running.", "Daemon 已在运行。")
                .to_owned();
            return Ok(());
        }

        let mut config = self.config.clone();
        config.host = self.config.host.trim().to_owned();
        config.port = self.config.port;
        self.notice = self
            .text("Starting daemon...", "正在启动 daemon...")
            .to_owned();
        match daemon::start(config).await {
            Ok(process) => {
                self.notice = match self.language {
                    TuiLanguage::English => format!(
                        "Daemon started on {} with pid {}. It will keep running after the TUI exits.",
                        process.listen_addr(), process.pid
                    ),
                    TuiLanguage::Chinese => format!(
                        "Daemon 已在 {} 启动，PID 为 {}；退出 TUI 后仍会继续运行。",
                        process.listen_addr(), process.pid
                    ),
                };
                self.last_error = None;
                self.push_log(self.notice.clone());
                self.daemon = Some(process);
            }
            Err(error) => {
                self.last_error = Some(error.to_string());
                self.notice = self
                    .text("Daemon failed to start.", "Daemon 启动失败。")
                    .to_owned();
                self.push_log(format!("{} {error}", self.notice.clone()));
            }
        }
        Ok(())
    }

    async fn stop_daemon(&mut self) -> Result<()> {
        self.notice = self
            .text("Stopping daemon...", "正在停止 daemon...")
            .to_owned();
        self.push_log(self.notice.clone());
        match daemon::stop(&self.config).await {
            Ok(Some(process)) => {
                self.notice = match self.language {
                    TuiLanguage::English => format!("Daemon stopped (pid {}).", process.pid),
                    TuiLanguage::Chinese => format!("Daemon 已停止（PID {}）。", process.pid),
                };
                self.last_error = None;
                self.push_log(self.notice.clone());
                self.daemon = None;
            }
            Ok(None) => {
                self.notice = self
                    .text("Daemon is already stopped.", "Daemon 已经停止。")
                    .to_owned();
                self.last_error = None;
                self.push_log(self.notice.clone());
                self.daemon = None;
            }
            Err(error) => {
                self.notice = self
                    .text("Daemon stop failed.", "Daemon 停止失败。")
                    .to_owned();
                self.last_error = Some(error.to_string());
                self.push_log(format!("{} {}", self.notice.clone(), error));
            }
        }
        Ok(())
    }

    async fn restart_daemon(&mut self) -> Result<()> {
        let mut config = self.config.clone();
        config.host = self.config.host.trim().to_owned();
        config.port = self.config.port;
        self.notice = self
            .text("Restarting daemon...", "正在重启 daemon...")
            .to_owned();
        self.push_log(self.notice.clone());
        match daemon::restart(config).await {
            Ok(process) => {
                self.notice = match self.language {
                    TuiLanguage::English => format!(
                        "Daemon restarted on {} with pid {}.",
                        process.listen_addr(),
                        process.pid
                    ),
                    TuiLanguage::Chinese => format!(
                        "Daemon 已在 {} 重启，PID 为 {}。",
                        process.listen_addr(),
                        process.pid
                    ),
                };
                self.last_error = None;
                self.push_log(self.notice.clone());
                self.daemon = Some(process);
            }
            Err(error) => {
                self.notice = self
                    .text("Daemon restart failed.", "Daemon 重启失败。")
                    .to_owned();
                self.last_error = Some(error.to_string());
                self.push_log(format!("{} {}", self.notice.clone(), error));
            }
        }
        Ok(())
    }

    fn refresh_daemon_status(&mut self) {
        let previous_pid = self.daemon.as_ref().map(|process| process.pid);
        match daemon::status(&self.config) {
            Ok(next) => {
                let next_pid = next.as_ref().map(|process| process.pid);
                if previous_pid != next_pid {
                    match next.as_ref() {
                        Some(process) => self.push_log(match self.language {
                            TuiLanguage::English => format!(
                                "Detected daemon pid {} on {}.",
                                process.pid,
                                process.listen_addr()
                            ),
                            TuiLanguage::Chinese => format!(
                                "检测到 daemon PID {}，监听地址为 {}。",
                                process.pid,
                                process.listen_addr()
                            ),
                        }),
                        None if previous_pid.is_some() => {
                            let line = self
                                .text("Daemon is no longer running.", "Daemon 已停止运行。")
                                .to_owned();
                            self.push_log(line)
                        }
                        None => {}
                    }
                }
                self.daemon = next;
            }
            Err(error) => {
                self.notice = self
                    .text("Failed to read daemon status.", "无法读取 daemon 状态。")
                    .to_owned();
                self.last_error = Some(error.to_string());
                self.push_log(format!("{} {}", self.notice.clone(), error));
            }
        }
    }

    fn start_host_edit(&mut self) {
        self.edit = Some(EditMode::Text {
            field: EditField::Host,
            value: self.config.host.clone(),
        });
        self.notice = self
            .text(
                "Listen IP edit window is open. Press Enter to apply or Esc to cancel.",
                "监听 IP 编辑窗口已打开。按 Enter 应用，按 Esc 取消。",
            )
            .to_owned();
    }

    fn start_port_edit(&mut self) {
        self.edit = Some(EditMode::Text {
            field: EditField::Port,
            value: self.config.port.to_string(),
        });
        self.notice = self
            .text(
                "Listen port edit window is open. Press Enter to apply or Esc to cancel.",
                "监听端口编辑窗口已打开。按 Enter 应用，按 Esc 取消。",
            )
            .to_owned();
    }

    fn start_workspace_root_picker(&mut self) {
        self.folder_picker = Some(FolderPicker::new(self.config.workspace_root.clone()));
        self.notice = self
            .text(
                "Workspace root selector is open. Enter descends; Space selects current directory.",
                "工作区根目录选择器已打开。Enter 进入目录，Space 选择当前目录。",
            )
            .to_owned();
    }

    fn start_pairing_encryption_edit(&mut self) {
        self.edit = Some(EditMode::Encryption {
            value: self.config.pairing_encryption,
        });
        self.notice = self
            .text(
                "Required encryption edit window is open. Use Left/Right to switch.",
                "强制加密编辑窗口已打开。使用左右方向键切换。",
            )
            .to_owned();
    }

    fn start_reset_edit(&mut self) {
        self.edit = Some(EditMode::Reset {
            target: ResetTarget::Auth,
        });
        self.notice = self
            .text(
                "Reset window is open. Use Left/Right to choose Auth or Encryption.",
                "重置窗口已打开。使用左右方向键选择认证或加密。",
            )
            .to_owned();
    }

    async fn apply_selected_workspace_root(&mut self) -> Result<()> {
        let Some(picker) = self.folder_picker.take() else {
            return Ok(());
        };

        match canonical_workspace_root(&picker.current) {
            Ok(root) => {
                self.config.workspace_root = root;
                let subject = self
                    .text("Workspace root updated", "工作区根目录已更新")
                    .to_owned();
                let saved = self.auto_save_settings(&subject);
                if saved && self.daemon.is_some() {
                    self.restart_daemon().await?;
                }
            }
            Err(error) => {
                self.folder_picker = Some(picker);
                self.last_error = Some(error.to_string());
                self.notice = self
                    .text("Workspace root was not changed.", "工作区根目录未更改。")
                    .to_owned();
            }
        }

        Ok(())
    }

    async fn commit_edit(&mut self) -> Result<()> {
        let Some(edit) = self.edit.take() else {
            return Ok(());
        };

        match edit {
            EditMode::Text {
                field: EditField::Host,
                value,
            } => match validate_host(&value) {
                Ok(host) => {
                    self.config.host = host;
                    let subject = self.text("Listen IP updated", "监听 IP 已更新").to_owned();
                    self.auto_save_settings(&subject);
                }
                Err(error) => {
                    self.last_error = Some(error);
                    self.notice = self
                        .text("Host was not changed.", "监听 IP 未更改。")
                        .to_owned();
                }
            },
            EditMode::Text {
                field: EditField::Port,
                value,
            } => match validate_port(&value) {
                Ok(port) => {
                    self.config.port = port;
                    let subject = self
                        .text("Listen port updated", "监听端口已更新")
                        .to_owned();
                    self.auto_save_settings(&subject);
                }
                Err(error) => {
                    self.last_error = Some(error);
                    self.notice = self
                        .text("Port was not changed.", "监听端口未更改。")
                        .to_owned();
                }
            },
            EditMode::Encryption { value } => {
                self.config.pairing_encryption = value;
                let subject = self
                    .text(
                        "Required transport encryption updated",
                        "强制传输加密已更新",
                    )
                    .to_owned();
                if self.auto_save_settings(&subject) {
                    self.finish_reset(&subject).await;
                }
            }
            EditMode::Reset { target } => self.reset_target(target).await?,
        }

        Ok(())
    }

    async fn reset_target(&mut self, target: ResetTarget) -> Result<()> {
        match target {
            ResetTarget::Auth => self.reset_auth().await,
            ResetTarget::Encryption => self.reset_encryption().await,
        }
    }

    async fn reset_auth(&mut self) -> Result<()> {
        match Config::reset_auth_token(self.config.data_dir.clone()) {
            Ok(token) => {
                self.config.security.auth_token = Some(token);
                let subject = self.text("Auth reset", "认证已重置").to_owned();
                self.finish_reset(&subject).await;
            }
            Err(error) => {
                self.notice = self.text("Auth reset failed.", "认证重置失败。").to_owned();
                self.last_error = Some(error.to_string());
                self.push_log(format!("{} {}", self.notice.clone(), error));
            }
        }
        Ok(())
    }

    async fn reset_encryption(&mut self) -> Result<()> {
        match PairingKeys::reset(&self.config.data_dir).await {
            Ok(_) => {
                let subject = self.text("Encryption reset", "加密密钥已重置").to_owned();
                self.finish_reset(&subject).await
            }
            Err(error) => {
                self.notice = self
                    .text("Encryption reset failed.", "加密密钥重置失败。")
                    .to_owned();
                self.last_error = Some(error.to_string());
                self.push_log(format!("{} {}", self.notice.clone(), error));
            }
        }
        Ok(())
    }

    async fn finish_reset(&mut self, subject: &str) {
        if self.daemon.is_none() {
            self.notice = match self.language {
                TuiLanguage::English => {
                    format!("{subject}. It will apply the next time the daemon starts.")
                }
                TuiLanguage::Chinese => {
                    format!("{subject}，将在 daemon 下次启动时生效。")
                }
            };
            self.last_error = None;
            self.push_log(self.notice.clone());
            return;
        }

        self.push_log(match self.language {
            TuiLanguage::English => {
                format!("{subject}; restarting daemon to apply it.")
            }
            TuiLanguage::Chinese => format!("{subject}；正在重启 daemon 以应用更改。"),
        });
        let mut config = self.config.clone();
        config.host = self.config.host.trim().to_owned();
        config.port = self.config.port;
        match daemon::restart(config).await {
            Ok(process) => {
                self.notice = match self.language {
                    TuiLanguage::English => format!(
                        "{subject} and daemon restarted on {} with pid {}.",
                        process.listen_addr(),
                        process.pid
                    ),
                    TuiLanguage::Chinese => format!(
                        "{subject}，daemon 已在 {} 重启，PID 为 {}。",
                        process.listen_addr(),
                        process.pid
                    ),
                };
                self.last_error = None;
                self.push_log(self.notice.clone());
                self.daemon = Some(process);
            }
            Err(error) => {
                self.notice = match self.language {
                    TuiLanguage::English => {
                        format!("{subject}, but daemon restart failed.")
                    }
                    TuiLanguage::Chinese => format!("{subject}，但 daemon 重启失败。"),
                };
                self.last_error = Some(error.to_string());
                self.push_log(format!("{} {}", self.notice.clone(), error));
            }
        }
    }

    fn auto_save_settings(&mut self, subject: &str) -> bool {
        let data_dir = self.config.data_dir.clone();
        let config_path = data_dir.join("config.toml");
        match Config::save_tui_settings(
            data_dir,
            &self.config.host,
            self.config.port,
            self.config.pairing_encryption,
            &self.config.workspace_root,
        ) {
            Ok(()) => {
                self.notice = match self.language {
                    TuiLanguage::English => {
                        format!("{subject} and auto-saved to {}.", config_path.display())
                    }
                    TuiLanguage::Chinese => {
                        format!("{subject}，并已自动保存到 {}。", config_path.display())
                    }
                };
                self.last_error = None;
                self.push_log(self.notice.clone());
                true
            }
            Err(error) => {
                self.notice = match self.language {
                    TuiLanguage::English => {
                        format!("{subject}, but settings auto-save failed.")
                    }
                    TuiLanguage::Chinese => format!("{subject}，但设置自动保存失败。"),
                };
                self.last_error = Some(error.to_string());
                self.push_log(format!("{} {}", self.notice.clone(), error));
                false
            }
        }
    }

    fn save_logs(&mut self) -> Result<()> {
        let dir = self.config.data_dir.join("tui-logs");
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create log directory {}", dir.display()))?;
        let timestamp = Utc::now().format("%Y%m%d-%H%M%S");
        let text_path = dir.join(format!("todex-tui-{timestamp}.log"));
        let jsonl_path = dir.join(format!("todex-tui-{timestamp}.jsonl"));

        let listen_host = self
            .daemon
            .as_ref()
            .map(|process| process.host.as_str())
            .unwrap_or(self.config.host.as_str());
        let listen_port = self
            .daemon
            .as_ref()
            .map(|process| process.port)
            .unwrap_or(self.config.port);
        let mut text = String::new();
        text.push_str("TodeX TUI log export\n");
        text.push_str(&format!("exported_at={}\n", Utc::now().to_rfc3339()));
        text.push_str(&format!("listen={listen_host}:{listen_port}\n"));
        text.push_str(&format!("data_dir={}\n", self.config.data_dir.display()));
        text.push_str(&format!(
            "workspace_root={}\n",
            self.config.workspace_root.display()
        ));
        text.push_str(&format!(
            "daemon_log={}\n",
            daemon::log_file_path(&self.config.data_dir).display()
        ));
        if let Some(process) = &self.daemon {
            text.push_str(&format!("daemon_pid={}\n", process.pid));
            text.push_str(&format!("daemon_started_at={}\n", process.started_at));
        }
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

        self.notice = match self.language {
            TuiLanguage::English => format!(
                "Auto-saved logs to {} and {}.",
                text_path.display(),
                jsonl_path.display()
            ),
            TuiLanguage::Chinese => format!(
                "日志已自动保存到 {} 和 {}。",
                text_path.display(),
                jsonl_path.display()
            ),
        };
        self.last_error = None;
        self.push_log(self.notice.clone());
        Ok(())
    }

    fn render(&self, frame: &mut Frame<'_>) {
        frame.render_widget(Clear, frame.area());
        if self.view == TuiView::Observer {
            self.render_observer(frame);
            self.dim_popup_background(frame);
            if self.pairing_qr.is_some() {
                let popup = self.pairing_qr_popup(frame.area());
                frame.render_widget(Clear, popup.area);
                frame.render_widget(
                    self.pairing_qr_paragraph(popup.lines, popup.title),
                    popup.area,
                );
            }
            if let Some(credentials) = &self.credentials {
                let area = self.credentials_area(frame.area());
                frame.render_widget(Clear, area);
                frame.render_widget(self.credentials_widget(credentials), area);
            }
            if self.device_pairing.open {
                self.render_device_pairing(frame);
            }
            return;
        }

        let chunks = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(if self.last_error.is_some() { 4 } else { 3 }),
            Constraint::Length(2),
        ])
        .margin(1)
        .spacing(1)
        .split(frame.area());
        let wide = chunks[0].width >= 68;
        let main = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(if wide { 33 } else { chunks[0].width }),
        ])
        .spacing(u16::from(wide))
        .split(chunks[0]);
        if wide {
            let status_height = if main[0].height >= 25 { 14 } else { 7 };
            let left = Layout::vertical([Constraint::Length(status_height), Constraint::Min(3)])
                .spacing(1)
                .split(main[0]);
            frame.render_widget(self.status_panel(left[0]), left[0]);
            frame.render_widget(self.log_panel(left[1]), left[1]);
        }
        let action_area = if wide {
            main[1]
        } else {
            let stacked = Layout::vertical([Constraint::Length(3), Constraint::Min(3)])
                .spacing(1)
                .split(main[1]);
            frame.render_widget(
                Paragraph::new(format!(
                    "{} · {}:{}",
                    self.text(
                        if self.daemon.is_some() {
                            "Running"
                        } else {
                            "Stopped"
                        },
                        if self.daemon.is_some() {
                            "运行中"
                        } else {
                            "已停止"
                        },
                    ),
                    self.config.host,
                    self.config.port
                ))
                .block(panel_block().title("TodeX Backend").borders(Borders::ALL)),
                stacked[0],
            );
            stacked[1]
        };
        let mut actions = ListState::default().with_selected(Some(self.selected_action));
        frame.render_stateful_widget(self.action_panel(), action_area, &mut actions);
        frame.render_widget(self.message_panel(), chunks[1]);
        frame.render_widget(self.help_panel(), chunks[2]);
        self.dim_popup_background(frame);
        if self.pairing_qr.is_some() {
            let popup = self.pairing_qr_popup(frame.area());
            frame.render_widget(Clear, popup.area);
            frame.render_widget(
                self.pairing_qr_paragraph(popup.lines, popup.title),
                popup.area,
            );
        }
        if let Some(edit) = &self.edit {
            let area = self.edit_popup_area(frame.area(), edit);
            frame.render_widget(Clear, area);
            frame.render_widget(self.edit_popup_paragraph(edit), area);
        }
        if let Some(picker) = &self.folder_picker {
            let area = self.folder_picker_area(frame.area());
            frame.render_widget(Clear, area);
            self.render_folder_picker(frame, area, picker);
        }
        if let Some(credentials) = &self.credentials {
            let area = self.credentials_area(frame.area());
            frame.render_widget(Clear, area);
            frame.render_widget(self.credentials_widget(credentials), area);
        }
        if self.device_pairing.open {
            self.render_device_pairing(frame);
        }
    }

    fn dim_popup_background(&self, frame: &mut Frame<'_>) {
        if self.pairing_qr.is_some()
            || self.credentials.is_some()
            || self.device_pairing.open
            || (self.view == TuiView::Control
                && (self.edit.is_some() || self.folder_picker.is_some()))
        {
            for cell in &mut frame.buffer_mut().content {
                cell.set_style(
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                );
            }
        }
    }

    fn render_observer(&self, frame: &mut Frame<'_>) {
        let chunks = Layout::vertical([
            Constraint::Length(6),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .margin(1)
        .spacing(1)
        .split(frame.area());
        let wide = chunks[1].width >= 90;
        let main_chunks = if wide {
            Layout::horizontal([Constraint::Length(34), Constraint::Min(0)])
                .spacing(1)
                .split(chunks[1])
        } else {
            Layout::vertical([Constraint::Length(5), Constraint::Min(3)])
                .spacing(1)
                .split(chunks[1])
        };
        let state = self.observer_state();
        frame.render_widget(self.observer_summary_panel(&state), chunks[0]);
        let mut sessions = ListState::default()
            .with_selected((!state.sessions.is_empty()).then_some(self.selected_session));
        frame.render_stateful_widget(self.session_panel(&state), main_chunks[0], &mut sessions);
        frame.render_widget(
            self.session_detail_panel(&state, main_chunks[1]),
            main_chunks[1],
        );
        frame.render_widget(self.observer_help_panel(), chunks[2]);
    }

    fn status_panel(&self, area: Rect) -> Paragraph<'_> {
        let process = self.daemon.as_ref();
        let status = if process.is_some() {
            Span::styled(
                self.text("Running", "运行中"),
                Style::default().fg(Color::Green),
            )
        } else if self.last_error.is_some() {
            Span::styled(self.text("Failed", "失败"), Style::default().fg(Color::Red))
        } else {
            Span::styled(
                self.text("Stopped", "已停止"),
                Style::default().fg(Color::Yellow),
            )
        };
        let uptime = process
            .and_then(|process| (Utc::now() - process.started_at).to_std().ok())
            .map(format_duration)
            .unwrap_or_else(|| "-".to_owned());
        let daemon_pid = process
            .map(|process| process.pid.to_string())
            .unwrap_or_else(|| "-".to_owned());
        let connection_state = if let Some(process) = process {
            Span::styled(
                match self.language {
                    TuiLanguage::English => format!("daemon pid {} listening", process.pid),
                    TuiLanguage::Chinese => format!("daemon 进程 {} 正在监听", process.pid),
                },
                Style::default().fg(Color::Cyan),
            )
        } else {
            Span::styled(
                self.text("offline", "离线"),
                Style::default().fg(Color::Yellow),
            )
        };
        let listen_host = process
            .map(|process| process.host.as_str())
            .unwrap_or(self.config.host.as_str());
        let listen_port = process
            .map(|process| process.port)
            .unwrap_or(self.config.port);
        let data_dir = process
            .map(|process| process.data_dir.as_path())
            .unwrap_or(self.config.data_dir.as_path());
        let workspace_root = process
            .map(|process| process.workspace_root.as_path())
            .unwrap_or(self.config.workspace_root.as_path());
        let token = self
            .config
            .security
            .auth_token
            .as_deref()
            .filter(|token| !token.trim().is_empty());
        let token_line = match token {
            Some(token) => Line::from(vec![
                Span::raw(self.text("Token: ", "令牌：")),
                Span::styled(token.to_owned(), Style::default().fg(Color::Green)),
            ]),
            None => Line::from(vec![
                Span::raw(self.text("Token: ", "令牌：")),
                Span::styled(
                    self.text("not set", "未设置"),
                    Style::default().fg(Color::Red),
                ),
            ]),
        };
        let auth_state = if self.config.security.enable_auth {
            Span::styled(
                self.text("enabled", "已启用"),
                Style::default().fg(Color::Yellow),
            )
        } else {
            Span::styled(
                self.text("disabled", "已禁用"),
                Style::default().fg(Color::Green),
            )
        };

        if area.height < 14 {
            return Paragraph::new(vec![
                Line::from(vec![Span::raw(self.text("Status: ", "状态：")), status]),
                Line::from(format!("{listen_host}:{listen_port}")),
                Line::from(format!(
                    "{}{}",
                    self.text("Encryption: ", "加密："),
                    pairing_encryption_label(self.config.pairing_encryption)
                )),
                Line::from(format!(
                    "{}{}",
                    self.text("Auth: ", "认证："),
                    auth_state.content
                )),
                Line::from(format!(
                    "{}{}  [d]",
                    self.text("Pending devices: ", "待验证设备："),
                    self.device_pairing.requests.len()
                )),
            ])
            .block(panel_block().title("TodeX Backend").borders(Borders::ALL));
        }

        Paragraph::new(vec![
            Line::from(vec![Span::raw(self.text("Status: ", "状态：")), status]),
            Line::styled(
                match self.language {
                    TuiLanguage::English => format!(
                        "Device verification: {} pending — press d",
                        self.device_pairing.requests.len()
                    ),
                    TuiLanguage::Chinese => format!(
                        "设备验证：{} 个待确认 — 按 d 核对验证码",
                        self.device_pairing.requests.len()
                    ),
                },
                Style::default()
                    .fg(if self.device_pairing.requests.is_empty() {
                        Color::Cyan
                    } else {
                        Color::Yellow
                    })
                    .add_modifier(Modifier::BOLD),
            ),
            Line::from(match self.language {
                TuiLanguage::English => format!("Listen: {listen_host}:{listen_port}"),
                TuiLanguage::Chinese => format!("监听：{listen_host}:{listen_port}"),
            }),
            Line::from(format!(
                "WS endpoint: ws://{listen_host}:{listen_port}/v2/ws"
            )),
            Line::from(match self.language {
                TuiLanguage::English => format!(
                    "Required encryption: {} (action e)",
                    pairing_encryption_label(self.config.pairing_encryption)
                ),
                TuiLanguage::Chinese => format!(
                    "强制加密：{}（操作 e）",
                    pairing_encryption_label(self.config.pairing_encryption)
                ),
            }),
            Line::from(self.text(
                "g pairing QR · c credentials · d device verification",
                "g 配对二维码 · c 凭据 · d 设备验证",
            )),
            Line::from(vec![Span::raw(self.text("Auth: ", "认证：")), auth_state]),
            token_line,
            Line::from(match self.language {
                TuiLanguage::English => format!("Data dir: {}", data_dir.display()),
                TuiLanguage::Chinese => format!("数据目录：{}", data_dir.display()),
            }),
            Line::from(match self.language {
                TuiLanguage::English => {
                    format!("Workspace root: {} (action w)", workspace_root.display())
                }
                TuiLanguage::Chinese => {
                    format!("工作区根目录：{}（操作 w）", workspace_root.display())
                }
            }),
            Line::from(match self.language {
                TuiLanguage::English => format!("Uptime: {uptime} | Daemon PID: {daemon_pid}"),
                TuiLanguage::Chinese => format!("运行时间：{uptime} | Daemon PID：{daemon_pid}"),
            }),
            Line::from(vec![
                Span::raw(self.text("Connection: ", "连接：")),
                connection_state,
            ]),
        ])
        .block(panel_block().title("TodeX Backend").borders(Borders::ALL))
    }

    fn log_panel(&self, area: ratatui::layout::Rect) -> Paragraph<'_> {
        let lines = if self.live_logs.is_empty() {
            vec![Line::from(
                self.text("No runtime events yet.", "暂无运行事件。"),
            )]
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
            match self.language {
                TuiLanguage::English => format!("Live Logs [follow {}]", lines.len()),
                TuiLanguage::Chinese => format!("实时日志 [跟随 {}]", lines.len()),
            }
        } else {
            match self.language {
                TuiLanguage::English => format!("Live Logs [{} / {}]", scroll, lines.len()),
                TuiLanguage::Chinese => format!("实时日志 [{} / {}]", scroll, lines.len()),
            }
        };

        Paragraph::new(lines)
            .scroll((scroll as u16, 0))
            .wrap(Wrap { trim: true })
            .block(panel_block().title(title).borders(Borders::ALL))
    }

    fn action_panel(&self) -> List<'_> {
        let start_stop = if self.daemon.is_some() {
            self.text("Stop daemon", "停止 daemon")
        } else {
            self.text("Start daemon", "启动 daemon")
        };
        let actions = [
            start_stop,
            self.text("Restart daemon", "重启 daemon"),
            self.text("Edit listen IP", "编辑监听 IP"),
            self.text("Edit listen port", "编辑监听端口"),
            self.text("Choose workspace root", "选择工作区根目录"),
            self.text("Edit required encryption", "编辑强制加密"),
            self.text("Reset", "重置"),
            self.text("Show pairing QR", "显示配对二维码"),
            self.text("Credentials & copy", "凭据与复制"),
            self.text("Language: English", "语言：中文"),
            self.text("Device verification", "设备验证"),
            self.text("Quit", "退出"),
        ];
        let shortcuts = ["s", "r", "h", "p", "w", "e", "x", "g", "c", "l", "d", "q"];
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
                ListItem::new(Line::from(format!("{marker}[{}] {label}", shortcuts[idx])))
                    .style(style)
            })
            .collect::<Vec<_>>();

        List::new(items).block(
            panel_block()
                .title(self.text("Actions", "操作"))
                .borders(Borders::ALL),
        )
    }

    fn help_panel(&self) -> Paragraph<'_> {
        Paragraph::new(vec![
            Line::from(self.text(
                "↑↓/j k select · Enter run · o observer · q quit",
                "↑↓/j k 选择 · Enter 执行 · o 观察器 · q 退出",
            )),
            Line::from(self.text(
                "PgUp/PgDn/Home/End logs · Settings saved · Quit keeps daemon online",
                "PgUp/PgDn/Home/End 日志 · 设置自动保存 · 退出保留后台服务",
            )),
        ])
    }

    fn message_panel(&self) -> Paragraph<'_> {
        let mut lines = Vec::new();
        if let Some(error) = &self.last_error {
            lines.push(Line::from(vec![
                Span::styled(
                    self.text("Error: ", "错误："),
                    Style::default().fg(Color::Red),
                ),
                Span::raw(error.clone()),
            ]));
        }

        lines.push(Line::from(self.notice.clone()));

        Paragraph::new(lines).wrap(Wrap { trim: true }).block(
            panel_block()
                .title(self.text("Messages", "消息"))
                .borders(Borders::ALL),
        )
    }

    fn credentials_area(&self, area: Rect) -> Rect {
        centered_area(area, 96, 24)
    }

    fn credentials_widget(&self, credentials: &CredentialsPopup) -> Paragraph<'static> {
        let selected = |index| {
            if credentials.selected == index {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            }
        };
        let unavailable = self.text("Not available", "不可用").to_owned();
        let token = credentials
            .auth_token
            .clone()
            .unwrap_or_else(|| unavailable.clone());
        let public_key = credentials
            .public_key
            .clone()
            .unwrap_or_else(|| unavailable.clone());
        let encryption = pairing_encryption_label(self.config.pairing_encryption);
        let token_marker = if credentials.selected == 0 {
            "> "
        } else {
            "  "
        };
        let key_marker = if credentials.selected == 1 {
            "> "
        } else {
            "  "
        };
        let lines = vec![
            Line::from(Span::styled(
                format!("{token_marker}{}", self.text("Auth Token", "认证令牌")),
                selected(0),
            )),
            Line::from(token),
            Line::from(""),
            Line::from(Span::styled(
                match self.language {
                    TuiLanguage::English => {
                        format!("{key_marker}Encryption public key ({encryption})")
                    }
                    TuiLanguage::Chinese => format!("{key_marker}加密公钥（{encryption}）"),
                },
                selected(1),
            )),
            Line::from(public_key),
            Line::from(""),
            Line::from(self.text(
                "Up/Down selects. Enter or c copies. PageUp/PageDown scrolls. Esc closes.",
                "上下方向键选择，Enter 或 c 复制，PageUp/PageDown 滚动，Esc 关闭。",
            )),
            Line::from(self.text(
                "Only the public encryption key is shown; private keys never appear here.",
                "这里只显示加密公钥，私钥绝不会在此展示。",
            )),
        ];

        Paragraph::new(lines)
            .scroll((credentials.scroll, 0))
            .wrap(Wrap { trim: false })
            .block(
                panel_block()
                    .title(self.text("Credentials & Copy", "凭据与复制"))
                    .borders(Borders::ALL),
            )
    }

    fn edit_popup_area(&self, area: Rect, edit: &EditMode) -> Rect {
        let height = match edit {
            EditMode::Text { .. } => 8,
            EditMode::Encryption { .. } => 9,
            EditMode::Reset { .. } => 10,
        };
        centered_area(area, EDIT_POPUP_WIDTH, height)
    }

    fn edit_popup_paragraph(&self, edit: &EditMode) -> Paragraph<'static> {
        let (title, lines) = match edit {
            EditMode::Text {
                field: EditField::Host,
                value,
            } => (
                self.text("Edit Listen IP", "编辑监听 IP").to_owned(),
                vec![
                    Line::from(vec![
                        Span::styled(
                            self.text("Listen IP: ", "监听 IP："),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            if value.is_empty() {
                                " ".to_owned()
                            } else {
                                value.clone()
                            },
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]),
                    Line::from(""),
                    Line::from(
                        self.text("IPv4 or IPv6 address only.", "仅支持 IPv4 或 IPv6 地址。"),
                    ),
                    Line::from(self.text(
                        "Enter applies and auto-saves. Esc cancels.",
                        "Enter 应用并自动保存，Esc 取消。",
                    )),
                ],
            ),
            EditMode::Text {
                field: EditField::Port,
                value,
            } => (
                self.text("Edit Listen Port", "编辑监听端口").to_owned(),
                vec![
                    Line::from(vec![
                        Span::styled(
                            self.text("Listen port: ", "监听端口："),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            if value.is_empty() {
                                " ".to_owned()
                            } else {
                                value.clone()
                            },
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]),
                    Line::from(""),
                    Line::from(self.text("Use a port from 1 to 65535.", "端口范围为 1 到 65535。")),
                    Line::from(self.text(
                        "Enter applies and auto-saves. Esc cancels.",
                        "Enter 应用并自动保存，Esc 取消。",
                    )),
                ],
            ),
            EditMode::Encryption { value } => (
                self.text("Required Transport Encryption", "强制传输加密")
                    .to_owned(),
                vec![
                    Line::from(""),
                    Line::from(vec![
                        Span::styled(
                            "[<]",
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw("  "),
                        Span::styled(
                            pairing_encryption_label(*value),
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw("  "),
                        Span::styled(
                            "[>]",
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]),
                    Line::from(""),
                    Line::from(self.text(
                        "Required for sessions. None allows optional encryption.",
                        "会话必须使用所选加密；None 允许可选加密。",
                    )),
                    Line::from(self.text(
                        "Enter saves and restarts a running daemon. Esc cancels.",
                        "Enter 保存并重启正在运行的 daemon，Esc 取消。",
                    )),
                ],
            ),
            EditMode::Reset { target } => (
                self.text("Reset", "重置").to_owned(),
                vec![
                    Line::from(""),
                    Line::from(vec![
                        Span::styled(
                            "[<]",
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw("  "),
                        Span::styled(
                            match (self.language, target) {
                                (TuiLanguage::Chinese, ResetTarget::Auth) => "认证",
                                (TuiLanguage::Chinese, ResetTarget::Encryption) => "加密",
                                _ => reset_target_label(*target),
                            },
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw("  "),
                        Span::styled(
                            "[>]",
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]),
                    Line::from(""),
                    Line::from(match (self.language, target) {
                        (TuiLanguage::Chinese, ResetTarget::Auth) => {
                            "生成新的认证令牌并替换已保存的值。"
                        }
                        (TuiLanguage::Chinese, ResetTarget::Encryption) => {
                            "重新生成 X25519 和 ML-KEM-768 配对密钥。"
                        }
                        _ => reset_target_description(*target),
                    }),
                    Line::from(self.text(
                        "Enter resets the selected item. Esc cancels.",
                        "Enter 重置所选项目，Esc 取消。",
                    )),
                    Line::from(self.text(
                        "A running daemon restarts automatically.",
                        "运行中的 daemon 会自动重启。",
                    )),
                ],
            ),
        };

        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(panel_block().title(title).borders(Borders::ALL))
    }

    fn folder_picker_area(&self, area: Rect) -> Rect {
        centered_area(area, 82, 24)
    }

    fn render_folder_picker(&self, frame: &mut Frame<'_>, area: Rect, picker: &FolderPicker) {
        let block = panel_block()
            .title(self.text("Choose Workspace Root", "选择工作区根目录"))
            .borders(Borders::ALL);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let sections = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(inner);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(format!(
                    "{}{}",
                    self.text("Current: ", "当前目录："),
                    picker.current.display()
                )),
                Line::styled(
                    picker.error.clone().unwrap_or_default(),
                    Style::default().fg(Color::Red),
                ),
            ]),
            sections[0],
        );
        let mut selection = ListState::default()
            .with_selected((!picker.entries.is_empty()).then_some(picker.selected));
        frame.render_stateful_widget(
            self.folder_picker_widget(picker),
            sections[1],
            &mut selection,
        );
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(self.text(
                    "Space selects current · Enter/→ opens · ←/Backspace parent",
                    "Space 选择当前目录 · Enter/→ 打开 · ←/Backspace 上级",
                )),
                Line::from(self.text(
                    "↑↓ navigate · Home home directory · Esc cancels",
                    "↑↓ 选择 · Home 主目录 · Esc 取消",
                )),
            ]),
            sections[2],
        );
    }

    fn folder_picker_widget(&self, picker: &FolderPicker) -> List<'static> {
        let mut items = Vec::new();
        if picker.entries.is_empty() {
            items.push(ListItem::new(Line::from(
                self.text("No readable subdirectories.", "没有可读取的子目录。"),
            )));
        } else {
            for (idx, entry) in picker.entries.iter().enumerate() {
                let marker = if idx == picker.selected { "> " } else { "  " };
                let style = if idx == picker.selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                items.push(
                    ListItem::new(Line::from(format!("{marker}{}/", entry.display_name())))
                        .style(style),
                );
            }
        }

        List::new(items)
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
            Some(qr) if qr.payloads.len() > 1 => match self.language {
                TuiLanguage::English => format!(
                    "Pairing QR {}/{} - Left/Right switch, b browser, Esc closes",
                    qr.active_index + 1,
                    qr.payloads.len()
                ),
                TuiLanguage::Chinese => format!(
                    "配对二维码 {}/{} - 左右切换，b 浏览器，Esc 关闭",
                    qr.active_index + 1,
                    qr.payloads.len()
                ),
            },
            Some(_) | None => self
                .text(
                    "Pairing QR - b browser, Esc closes",
                    "配对二维码 - b 浏览器，Esc 关闭",
                )
                .to_owned(),
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
            return vec![Line::from(
                self.text("Failed to render pairing QR.", "无法生成配对二维码。"),
            )];
        };
        let Some(payload) = qr.payloads.get(qr.active_index) else {
            return vec![Line::from(
                self.text("Failed to render pairing QR.", "无法生成配对二维码。"),
            )];
        };
        match render_qr_text_for_bounds(payload, max_width / 2, max_height / 2) {
            Ok(rendered)
                if rendered.width.saturating_mul(2) <= max_width
                    && rendered.height.saturating_mul(2) <= max_height =>
            {
                // Decode the compact representation into complete background cells.
                // Glyphs such as ▀/▄/█ can leave gaps with terminal font leading;
                // backgrounds cover the entire cell regardless of font metrics.
                rendered
                    .text
                    .lines()
                    .flat_map(|line| {
                        (0..2).map(move |half| {
                            Line::from(
                                line.chars()
                                    .map(|cell| {
                                        let dark =
                                            matches!((cell, half), ('█', _) | ('▀', 0) | ('▄', 1));
                                        Span::styled(
                                            "  ",
                                            Style::reset().fg(Color::Rgb(0, 0, 0)).bg(if dark {
                                                Color::Rgb(0, 0, 0)
                                            } else {
                                                Color::Rgb(255, 255, 255)
                                            }),
                                        )
                                    })
                                    .collect::<Vec<_>>(),
                            )
                        })
                    })
                    .collect()
            }
            Ok(rendered) => vec![
                Line::from(self.text(
                    "Terminal is too small for the pairing QR.",
                    "终端尺寸太小，无法显示配对二维码。",
                )),
                Line::from(match self.language {
                    TuiLanguage::English => format!(
                        "Need at least {}x{} cells for the code.",
                        rendered.width.saturating_mul(2),
                        rendered.height.saturating_mul(2)
                    ),
                    TuiLanguage::Chinese => format!(
                        "二维码至少需要 {}x{} 个字符单元。",
                        rendered.width.saturating_mul(2),
                        rendered.height.saturating_mul(2)
                    ),
                }),
                Line::from(self.text(
                    "Press b to view in a browser, or enlarge the terminal.",
                    "按 b 在浏览器查看，或放大终端窗口。",
                )),
            ],
            Err(error) => vec![
                Line::from(self.text("Failed to render pairing QR.", "无法生成配对二维码。")),
                Line::from(error.to_string()),
            ],
        }
    }

    fn pairing_qr_paragraph(&self, lines: Vec<Line<'static>>, title: String) -> Paragraph<'static> {
        // ANSI Black/White are user-remappable palette entries. Force actual
        // black ink on white paper, and clear inherited inverse/dim modifiers.
        let qr_style = Style::reset()
            .fg(Color::Rgb(0, 0, 0))
            .bg(Color::Rgb(255, 255, 255));
        Paragraph::new(lines).style(qr_style).block(
            panel_block()
                .title(title)
                .borders(Borders::ALL)
                .style(qr_style),
        )
    }

    fn observer_state(&self) -> ObserverState {
        ObserverState::from_events(&self.live_events)
    }

    fn observer_summary_panel(&self, state: &ObserverState) -> Paragraph<'_> {
        let runtime_state = if self.daemon.is_some() {
            self.text("running", "运行中")
        } else {
            self.text("stopped", "已停止")
        };
        let selected = state
            .sessions
            .get(self.selected_session)
            .map(|session| session.session_id.as_str())
            .unwrap_or("-");

        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    self.text("Observer ", "观察器 "),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(self.text(
                    "read-only view for this TUI session",
                    "当前 TUI 会话的只读视图",
                )),
            ]),
            Line::from(match self.language {
                TuiLanguage::English => format!(
                    "{runtime_state} · Sessions {} · Tasks {} · Requests {} · Events {}",
                    state.sessions.len(),
                    state.active_task_count,
                    state.pending_request_count,
                    self.live_events.len()
                ),
                TuiLanguage::Chinese => format!(
                    "服务：{runtime_state} | 会话：{} | 活跃任务：{} | 待处理请求：{} | 事件：{}",
                    state.sessions.len(),
                    state.active_task_count,
                    state.pending_request_count,
                    self.live_events.len()
                ),
            }),
            Line::from(match self.language {
                TuiLanguage::English => format!("Selected session: {selected}"),
                TuiLanguage::Chinese => format!("所选会话：{selected}"),
            }),
            Line::from(self.text(
                "No service commands are available on this page.",
                "此页面不提供服务控制命令。",
            )),
        ])
        .wrap(Wrap { trim: true })
        .block(
            panel_block()
                .title(self.text("Session Observer", "会话观察器"))
                .borders(Borders::ALL),
        )
    }

    fn session_panel(&self, state: &ObserverState) -> List<'_> {
        let items = if state.sessions.is_empty() {
            vec![ListItem::new(Line::from(self.text(
                "No Codex session events yet.",
                "暂无 Codex 会话事件。",
            )))]
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
                    ListItem::new(vec![
                        Line::from(format!(
                            "{marker}{}",
                            truncate_text(&session.session_id, 22)
                        )),
                        Line::from(match self.language {
                            TuiLanguage::English => format!(
                                "  {} ev · {} turns · {} req",
                                session.event_count,
                                session.turns.len(),
                                session.pending_request_count()
                            ),
                            TuiLanguage::Chinese => format!(
                                "  {} 事件 · {} 轮次 · {} 请求",
                                session.event_count,
                                session.turns.len(),
                                session.pending_request_count()
                            ),
                        }),
                    ])
                    .style(style)
                })
                .collect()
        };

        List::new(items).block(
            panel_block()
                .title(self.text("Sessions", "会话"))
                .borders(Borders::ALL),
        )
    }

    fn session_detail_panel(&self, state: &ObserverState, area: Rect) -> Paragraph<'_> {
        let lines = match state.sessions.get(self.selected_session) {
            Some(session) => session_detail_lines(session, self.language),
            None => vec![Line::from(self.text(
                "Start or connect a client to populate this observer.",
                "启动或连接客户端后，此观察器将显示会话内容。",
            ))],
        };
        let content_height = area.height.saturating_sub(2) as usize;
        let max_scroll = lines.len().saturating_sub(content_height);
        let scroll = self.observer_scroll.min(max_scroll);
        let line_count = lines.len();

        Paragraph::new(lines)
            .scroll((scroll as u16, 0))
            .wrap(Wrap { trim: true })
            .block(
                panel_block()
                    .title(match self.language {
                        TuiLanguage::English => {
                            format!("History / Conversation / Current Tasks [{line_count}]")
                        }
                        TuiLanguage::Chinese => {
                            format!("历史 / 对话 / 当前任务 [{line_count}]")
                        }
                    })
                    .borders(Borders::ALL),
            )
    }

    fn observer_help_panel(&self) -> Paragraph<'_> {
        Paragraph::new(vec![
            Line::from(self.text(
                "↑↓ select session · PgUp/PgDn scroll · Home/End jump",
                "↑↓ 选择会话 · PgUp/PgDn 滚动 · Home/End 跳转",
            )),
            Line::from(self.text(
                "q / Esc / o / Tab return · Read-only view",
                "q / Esc / o / Tab 返回 · 只读视图",
            )),
        ])
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

fn session_detail_lines(session: &ObservedSession, language: TuiLanguage) -> Vec<Line<'static>> {
    let text = |english: &'static str, chinese: &'static str| language.text(english, chinese);
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            text("Session: ", "会话："),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(session.session_id.clone()),
    ]));
    lines.push(Line::from(match language {
        TuiLanguage::English => format!(
            "First event: {} | Last event: {} | Events: {}",
            session.first_time.format("%H:%M:%S"),
            session.last_time.format("%H:%M:%S"),
            session.event_count
        ),
        TuiLanguage::Chinese => format!(
            "首个事件：{} | 最近事件：{} | 事件数：{}",
            session.first_time.format("%H:%M:%S"),
            session.last_time.format("%H:%M:%S"),
            session.event_count
        ),
    }));
    lines.push(Line::from(match language {
        TuiLanguage::English => format!(
            "Adapter: {} | In-flight: {} | Child PID: {}",
            session.adapter_state.as_deref().unwrap_or("-"),
            session.in_flight_command_id.as_deref().unwrap_or("-"),
            session.child_pid.as_deref().unwrap_or("-")
        ),
        TuiLanguage::Chinese => format!(
            "适配器：{} | 执行中：{} | 子进程 PID：{}",
            session.adapter_state.as_deref().unwrap_or("-"),
            session.in_flight_command_id.as_deref().unwrap_or("-"),
            session.child_pid.as_deref().unwrap_or("-")
        ),
    }));
    lines.push(Line::from(match language {
        TuiLanguage::English => format!(
            "Threads: {} | Turns: {} | Pending requests: {} | Cloud tasks: {}",
            session.threads.len(),
            session.turns.len(),
            session.pending_request_count(),
            session.cloud_tasks.len()
        ),
        TuiLanguage::Chinese => format!(
            "线程：{} | 轮次：{} | 待处理请求：{} | 云任务：{}",
            session.threads.len(),
            session.turns.len(),
            session.pending_request_count(),
            session.cloud_tasks.len()
        ),
    }));

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        text("Current Running Tasks", "当前运行任务"),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    let mut active_lines = Vec::new();
    if let Some(command_id) = &session.in_flight_command_id {
        active_lines.push(match language {
            TuiLanguage::English => format!("command in flight: {command_id}"),
            TuiLanguage::Chinese => format!("执行中的命令：{command_id}"),
        });
    }
    active_lines.extend(
        session
            .turns
            .values()
            .filter(|turn| turn.status.is_active())
            .map(|turn| match language {
                TuiLanguage::English => format!("turn {} ({})", turn.turn_id, turn.last_event),
                TuiLanguage::Chinese => format!("轮次 {}（{}）", turn.turn_id, turn.last_event),
            }),
    );
    active_lines.extend(
        session
            .requests
            .values()
            .filter(|request| request.status == ObservedRequestStatus::Pending)
            .map(|request| match language {
                TuiLanguage::English => format!(
                    "pending request {} ({})",
                    request.request_id, request.request_type
                ),
                TuiLanguage::Chinese => format!(
                    "待处理请求 {}（{}）",
                    request.request_id, request.request_type
                ),
            }),
    );
    active_lines.extend(
        session
            .cloud_tasks
            .iter()
            .filter(|(_, status)| !terminal_status(status))
            .map(|(task_id, status)| match language {
                TuiLanguage::English => format!("cloud task {task_id} ({status})"),
                TuiLanguage::Chinese => format!("云任务 {task_id}（{status}）"),
            }),
    );
    if active_lines.is_empty() {
        lines.push(Line::from(text(
            "No active task inferred from current events.",
            "当前事件中未发现活跃任务。",
        )));
    } else {
        lines.extend(active_lines.into_iter().map(Line::from));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        text("Conversation Records", "对话记录"),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    if session.conversation.is_empty() {
        lines.push(Line::from(text(
            "No conversation item events observed yet.",
            "尚未观察到对话项目事件。",
        )));
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
        text("Plans", "计划"),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    if session.plans.is_empty() {
        lines.push(Line::from(text(
            "No plan updates observed yet.",
            "尚未观察到计划更新。",
        )));
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
        text("Requests", "请求"),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    if session.requests.is_empty() {
        lines.push(Line::from(text(
            "No request records observed yet.",
            "尚未观察到请求记录。",
        )));
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
        text("Full Event History", "完整事件历史"),
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

fn centered_area(area: Rect, desired_width: u16, desired_height: u16) -> Rect {
    let width = desired_width.min(area.width.saturating_sub(2)).max(1);
    let height = desired_height.min(area.height.saturating_sub(2)).max(1);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
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

struct FolderPicker {
    current: PathBuf,
    entries: Vec<FolderEntry>,
    selected: usize,
    error: Option<String>,
}

impl FolderPicker {
    fn new(start: PathBuf) -> Self {
        let current = canonical_workspace_root(&start)
            .ok()
            .or_else(|| {
                start
                    .parent()
                    .and_then(|parent| canonical_workspace_root(parent).ok())
            })
            .unwrap_or_else(default_picker_root);
        let mut picker = Self {
            current,
            entries: Vec::new(),
            selected: 0,
            error: None,
        };
        picker.refresh();
        picker
    }

    fn refresh(&mut self) {
        match read_directory_entries(&self.current) {
            Ok(entries) => {
                self.entries = entries;
                self.selected = self.selected.min(self.entries.len().saturating_sub(1));
                self.error = None;
            }
            Err(error) => {
                self.entries.clear();
                self.selected = 0;
                self.error = Some(error);
            }
        }
    }

    fn select_next(&mut self) {
        self.select_next_by(1);
    }

    fn select_next_by(&mut self, amount: usize) {
        if !self.entries.is_empty() {
            self.selected = (self.selected + amount).min(self.entries.len() - 1);
        }
    }

    fn open_selected(&mut self) {
        let Some(entry) = self.entries.get(self.selected) else {
            return;
        };
        self.current = entry.path.clone();
        self.selected = 0;
        self.refresh();
    }

    fn open_parent(&mut self) {
        let Some(parent) = self.current.parent() else {
            return;
        };
        self.current = parent.to_path_buf();
        self.selected = 0;
        self.refresh();
    }

    fn open_home(&mut self) {
        self.current = default_picker_root();
        self.selected = 0;
        self.refresh();
    }
}

struct FolderEntry {
    path: PathBuf,
    name: String,
}

impl FolderEntry {
    fn display_name(&self) -> &str {
        self.name.as_str()
    }
}

fn read_directory_entries(path: &Path) -> std::result::Result<Vec<FolderEntry>, String> {
    let mut entries = Vec::new();
    let read_dir = fs::read_dir(path)
        .map_err(|error| format!("failed to read {}: {}", path.display(), error))?;
    for entry in read_dir {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().to_string();
        if name.is_empty() || name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if !metadata.is_dir() {
            continue;
        }
        let canonical = match fs::canonicalize(&path) {
            Ok(path) => path,
            Err(_) => continue,
        };
        entries.push(FolderEntry {
            path: canonical,
            name,
        });
    }
    entries.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
    });
    Ok(entries)
}

fn default_picker_root() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"))
}

enum EditMode {
    Text { field: EditField, value: String },
    Encryption { value: PairingEncryption },
    Reset { target: ResetTarget },
}

enum EditField {
    Host,
    Port,
}

#[derive(Clone, Copy)]
enum ResetTarget {
    Auth,
    Encryption,
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
        PairingEncryption::None => "None",
        PairingEncryption::MlKem768 => "ML-KEM-768",
        PairingEncryption::X25519 => "X25519",
    }
}

fn previous_pairing_encryption(value: PairingEncryption) -> PairingEncryption {
    match value {
        PairingEncryption::None => PairingEncryption::X25519,
        PairingEncryption::X25519 => PairingEncryption::MlKem768,
        PairingEncryption::MlKem768 => PairingEncryption::None,
    }
}

fn reset_target_label(target: ResetTarget) -> &'static str {
    match target {
        ResetTarget::Auth => "Auth",
        ResetTarget::Encryption => "Encryption",
    }
}

fn reset_target_description(target: ResetTarget) -> &'static str {
    match target {
        ResetTarget::Auth => "Generates a new bearer token and updates config.toml.",
        ResetTarget::Encryption => "Regenerates X25519 and ML-KEM-768 pairing keys.",
    }
}

fn next_reset_target(target: ResetTarget) -> ResetTarget {
    match target {
        ResetTarget::Auth => ResetTarget::Encryption,
        ResetTarget::Encryption => ResetTarget::Auth,
    }
}

fn previous_reset_target(target: ResetTarget) -> ResetTarget {
    next_reset_target(target)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        validate_host, validate_port, ObservedRequestStatus, ObserverState, TuiLanguage,
        ACTION_COUNT,
    };
    use crate::event::EventRecord;

    fn render_preview(app: &super::TuiApp, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn write_preview(buffer: &ratatui::buffer::Buffer, name: &str) {
        if std::env::var_os("TODEX_TUI_PREVIEW").is_none() {
            return;
        }
        let width = buffer.area.width;
        let height = buffer.area.height;
        let mut cells = Vec::new();
        let mut rows = Vec::new();
        for y in 0..height {
            let mut row = String::new();
            for x in 0..width {
                let cell = &buffer[(x, y)];
                row.push_str(cell.symbol());
                cells.push(json!({
                    "x": x, "y": y, "symbol": cell.symbol(),
                    "fg": format!("{:?}", cell.fg), "bg": format!("{:?}", cell.bg),
                }));
            }
            rows.push(row);
        }
        let path = format!("/tmp/todex-tui-preview-{name}-{width}x{height}");
        std::fs::write(format!("{path}.txt"), rows.join("\n")).unwrap();
        std::fs::write(
            format!("{path}.json"),
            serde_json::to_string(&json!({ "width": width, "height": height, "cells": cells }))
                .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn responsive_views_keep_selected_actions_and_borders_visible() {
        for language in [TuiLanguage::English, TuiLanguage::Chinese] {
            let mut app = super::TuiApp::new(crate::config::Config::default());
            app.language = language;
            app.config.security.auth_token = Some("preview-token".into());
            app.config.data_dir = "/data/todex".into();
            app.config.workspace_root = "/workspace".into();
            app.notice = app
                .text(
                    "Ready. Settings are saved automatically.",
                    "就绪。设置会自动保存。",
                )
                .into();
            for (width, height) in [(40, 16), (80, 24), (100, 30), (120, 40)] {
                for selected in 0..ACTION_COUNT {
                    app.selected_action = selected;
                    let buffer = render_preview(&app, width, height);
                    let contents = buffer
                        .content
                        .iter()
                        .map(|cell| cell.symbol())
                        .collect::<String>();
                    let shortcut =
                        ["s", "r", "h", "p", "w", "e", "x", "g", "c", "l", "d", "q"][selected];
                    assert!(
                        contents.contains(&format!("> [{shortcut}]")),
                        "selected {selected} missing at {width}x{height}"
                    );
                    // Each framed row starts and ends at fixed columns. Frame
                    // glyphs themselves must not depend on ambiguous widths.
                    assert!(!contents
                        .chars()
                        .any(|ch| ('\u{2500}'..='\u{257f}').contains(&ch)));
                    assert!(contents.matches('+').count() >= 12);
                    if width >= 80 {
                        assert!(contents.contains("[s]"));
                        assert!(contents.contains("[q]"));
                    }
                }
                if std::env::var_os("TODEX_TUI_PREVIEW").is_some() {
                    for view in [super::TuiView::Control, super::TuiView::Observer] {
                        app.view = view;
                        let buffer = render_preview(&app, width, height);
                        write_preview(&buffer, &format!("{language:?}-{view:?}"));
                    }
                    app.view = super::TuiView::Control;
                }
            }
        }
    }

    #[test]
    fn popup_previews_preserve_selection_and_error_feedback() {
        for language in [TuiLanguage::English, TuiLanguage::Chinese] {
            let mut app = super::TuiApp::new(crate::config::Config::default());
            app.language = language;
            app.config.security.auth_token = Some("preview-token".into());
            app.notice = "A very long preview notice that should not hide an error. ".repeat(6);
            app.last_error = Some("Preview error".into());
            let buffer = render_preview(&app, 80, 24);
            assert!(buffer
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
                .contains("Preview error"));
            app.last_error = None;
            for popup in ["Encryption", "Reset", "Folder", "Credentials", "Device"] {
                app.edit = None;
                app.folder_picker = None;
                app.credentials = None;
                app.device_pairing.open = false;
                match popup {
                    "Encryption" => {
                        app.edit = Some(super::EditMode::Encryption {
                            value: app.config.pairing_encryption,
                        })
                    }
                    "Reset" => {
                        app.edit = Some(super::EditMode::Reset {
                            target: super::ResetTarget::Auth,
                        })
                    }
                    "Folder" => {
                        app.folder_picker = Some(super::FolderPicker {
                            current: "/workspace".into(),
                            entries: (0..30)
                                .map(|i| super::FolderEntry {
                                    path: format!("/workspace/folder-{i:02}").into(),
                                    name: format!("folder-{i:02}"),
                                })
                                .collect(),
                            selected: 29,
                            error: None,
                        })
                    }
                    "Credentials" => {
                        app.credentials = Some(super::CredentialsPopup {
                            auth_token: Some("preview-token".into()),
                            public_key: Some("preview-public-key".into()),
                            selected: 1,
                            scroll: 0,
                        })
                    }
                    "Device" => {
                        app.device_pairing.open = true;
                        app.device_pairing.replace(vec![device_request("preview")]);
                        app.device_pairing.navigate(true);
                    }
                    _ => unreachable!(),
                }
                let buffer = render_preview(&app, 80, 24);
                if popup == "Folder" {
                    let contents = buffer
                        .content
                        .iter()
                        .map(|cell| cell.symbol())
                        .collect::<String>();
                    assert!(contents.contains("/workspace"));
                    assert!(contents.contains("Space"));
                    assert!(buffer
                        .content
                        .iter()
                        .map(|cell| cell.symbol())
                        .collect::<String>()
                        .contains("> folder-29/"));
                }
                write_preview(&buffer, &format!("{language:?}-{popup}"));
            }
        }
    }

    #[test]
    fn observer_scrolls_to_the_selected_session() {
        for language in [TuiLanguage::English, TuiLanguage::Chinese] {
            let mut app = super::TuiApp::new(crate::config::Config::default());
            app.language = language;
            let state = super::ObserverState {
                sessions: (0..20)
                    .map(|index| super::ObservedSession::new(format!("session-{index:02}")))
                    .collect(),
                ..Default::default()
            };
            app.selected_session = 19;
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(34, 5)).unwrap();
            terminal
                .draw(|frame| {
                    let mut selection = ratatui::widgets::ListState::default()
                        .with_selected(Some(app.selected_session));
                    frame.render_stateful_widget(
                        app.session_panel(&state),
                        frame.area(),
                        &mut selection,
                    );
                })
                .unwrap();
            let contents = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(contents.contains("> session-19"));
        }
    }

    fn device_request(id: &str) -> super::DevicePairingRequest {
        super::DevicePairingRequest {
            request_id: id.to_owned(),
            verification_code: "ABCDE-FGHJK".to_owned(),
            device_name: "My laptop".to_owned(),
            expires_at: chrono::Utc::now().timestamp_millis() + 60_000,
        }
    }

    #[test]
    fn device_selection_preserves_identity_and_never_moves_to_a_new_request_implicitly() {
        let mut state = super::DevicePairingState::default();
        state.replace(vec![device_request("first"), device_request("second")]);
        state.navigate(true);
        assert_eq!(state.selected().unwrap().request_id, "first");
        state.replace(vec![
            device_request("new"),
            device_request("first"),
            device_request("second"),
        ]);
        assert_eq!(state.selected().unwrap().request_id, "first");
        state.replace(vec![device_request("new"), device_request("second")]);
        assert!(state.selected().is_none());
        state.navigate(true);
        assert_eq!(state.selected().unwrap().request_id, "new");
    }

    #[test]
    fn device_names_cannot_inject_terminal_control_or_bidi_sequences() {
        assert_eq!(
            super::device_display_name("Laptop\n\r\t\u{1b}\u{202e}test\u{2069}"),
            "Laptoptest"
        );
        assert_eq!(super::device_display_name(&"a".repeat(200)).len(), 100);
    }

    #[test]
    fn device_verification_enter_does_not_approve_and_escape_only_closes_popup() {
        let mut app = super::TuiApp::new(crate::config::Config::default());
        app.device_pairing.open = true;
        app.device_pairing.replace(vec![device_request("pending")]);
        app.device_pairing.navigate(true);
        app.handle_device_pairing_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(app.device_pairing.open);
        assert_eq!(app.device_pairing.selected().unwrap().request_id, "pending");
        assert!(app.device_pairing.refreshed_at.is_none());
        app.handle_device_pairing_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(!app.device_pairing.open);
    }

    #[test]
    fn device_approval_requires_code_visible_in_the_current_terminal() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = super::TuiApp::new(crate::config::Config::default());
        app.language = super::TuiLanguage::English;
        app.device_pairing.open = true;
        app.device_pairing.replace(vec![device_request("pending")]);
        app.device_pairing.navigate(true);
        let mut full = Terminal::new(TestBackend::new(100, 28)).unwrap();
        full.draw(|frame| app.render_device_pairing(frame)).unwrap();
        assert!(app.device_pairing.displayed_request.borrow().is_some());
        for (width, height) in [(30, 28), (100, 12)] {
            let mut small = Terminal::new(TestBackend::new(width, height)).unwrap();
            small
                .draw(|frame| app.render_device_pairing(frame))
                .unwrap();
            assert!(app.device_pairing.displayed_request.borrow().is_none());
            app.handle_device_pairing_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('a'),
                crossterm::event::KeyModifiers::NONE,
            ));
            // No filesystem refresh/write occurred, and approval was not recorded.
            assert!(app.device_pairing.refreshed_at.is_none());
            assert_eq!(app.device_pairing.selected().unwrap().request_id, "pending");
            assert!(app
                .device_pairing
                .error
                .as_ref()
                .unwrap()
                .contains("full code"));
        }
    }

    #[test]
    fn device_verification_renders_complete_code_name_and_expiry() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = super::TuiApp::new(crate::config::Config::default());
        app.language = super::TuiLanguage::English;
        app.device_pairing.open = true;
        app.device_pairing.replace(vec![device_request("pending")]);
        app.device_pairing.navigate(true);
        let mut terminal = Terminal::new(TestBackend::new(100, 28)).unwrap();
        terminal
            .draw(|frame| app.render_device_pairing(frame))
            .unwrap();
        let contents = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(contents.contains("ABCDE-FGHJK"));
        assert!(contents.contains("My laptop"));
        assert!(contents.contains("Expires in"));
        assert!(contents.contains("a approve"));
    }

    #[test]
    fn pairing_qr_preserves_modules_and_true_colors_in_terminal_buffer() {
        use qrcodegen::{QrCode, QrCodeEcc};
        use ratatui::{
            backend::TestBackend,
            style::{Color, Modifier, Style},
            widgets::Paragraph,
            Terminal,
        };
        let payload = "todex-pairing-test:no-real-credentials";
        let qr = QrCode::encode_text(payload, QrCodeEcc::Low).unwrap();
        let mut config = crate::config::Config::default();
        config.data_dir =
            std::env::temp_dir().join(format!("todex-qr-test-{}", uuid::Uuid::new_v4()));
        let mut app = super::TuiApp::new(config);
        app.pairing_qr = Some(super::PairingQr {
            payloads: vec![payload.to_owned()],
            active_index: 0,
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 60)).unwrap();
        let popup = app.pairing_qr_popup(ratatui::layout::Rect::new(0, 0, 100, 60));
        let qr_area = popup.area;
        terminal
            .draw(|frame| {
                // Simulate hostile inherited terminal styles, including inverse text.
                frame.render_widget(
                    Paragraph::new("background").style(
                        Style::default()
                            .fg(Color::White)
                            .bg(Color::Black)
                            .add_modifier(Modifier::REVERSED | Modifier::DIM),
                    ),
                    frame.area(),
                );
                frame.render_widget(
                    app.pairing_qr_paragraph(popup.lines.clone(), popup.title.clone()),
                    qr_area,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        for y in qr_area.top()..qr_area.bottom() {
            for x in qr_area.left()..qr_area.right() {
                let cell = &buffer[(x, y)];
                assert_eq!(cell.fg, Color::Rgb(0, 0, 0));
                assert!(matches!(
                    cell.bg,
                    Color::Rgb(0, 0, 0) | Color::Rgb(255, 255, 255)
                ));
                assert!(!cell.modifier.intersects(Modifier::REVERSED | Modifier::DIM));
            }
        }
        // Every module consists of two ordinary spaces with a solid background.
        // This remains contiguous even if the terminal adds font leading.
        for y in 0..qr.size() + 8 {
            for x in 0..qr.size() + 8 {
                for column in 0..2 {
                    let cell = &buffer[(
                        qr_area.x + 1 + (x * 2 + column) as u16,
                        qr_area.y + 1 + y as u16,
                    )];
                    assert_eq!(cell.symbol(), " ");
                    assert_eq!(
                        cell.bg == Color::Rgb(0, 0, 0),
                        qr.get_module(x - 4, y - 4),
                        "QR module at {x},{y}"
                    );
                }
            }
        }
    }

    #[test]
    fn pairing_qr_does_not_shrink_quiet_zone_to_fit_a_small_terminal() {
        use crate::transport_crypto::render_qr_text_for_bounds;
        let payload = "synthetic-qr-payload";
        let full = render_qr_text_for_bounds(payload, 100, 60).unwrap();
        let small = render_qr_text_for_bounds(payload, full.width - 1, full.height - 1).unwrap();
        assert_eq!(full, small);
        let mut config = crate::config::Config::default();
        config.data_dir =
            std::env::temp_dir().join(format!("todex-qr-test-{}", uuid::Uuid::new_v4()));
        let mut app = super::TuiApp::new(config);
        app.language = super::TuiLanguage::English;
        app.pairing_qr = Some(super::PairingQr {
            payloads: vec![payload.to_owned()],
            active_index: 0,
        });
        let fits = app.pairing_qr_lines(full.width * 2, full.height * 2);
        assert_eq!(fits.len(), usize::from(full.height * 2));
        assert!(fits
            .iter()
            .all(|line| line.width() == usize::from(full.width * 2)));
        let lines = app.pairing_qr_lines(full.width * 2 - 1, full.height * 2 - 1);
        assert!(lines[0].to_string().contains("too small"));
        assert!(!lines
            .iter()
            .any(|line| line.to_string().contains(['█', '▀', '▄'])));
    }

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
    fn tui_language_parses_persisted_values() {
        assert_eq!(TuiLanguage::parse("zh-CN"), Some(TuiLanguage::Chinese));
        assert_eq!(TuiLanguage::parse("zh-tw"), Some(TuiLanguage::Chinese));
        assert_eq!(TuiLanguage::parse("en"), Some(TuiLanguage::English));
        assert_eq!(TuiLanguage::parse("invalid"), None);
        assert_eq!(TuiLanguage::Chinese.as_str(), "zh-CN");
        assert_eq!(TuiLanguage::English.as_str(), "en");
        assert_eq!(ACTION_COUNT, 12);
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
