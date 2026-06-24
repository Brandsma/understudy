//! ratatui TUI: async event loop, session picker, live activity feed (Tier-1 summary),
//! and a streaming comprehension chat panel. (Thinking/detail panels are the next phase.)

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event as CEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::layout::Position;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::DefaultTerminal;
use tokio::sync::mpsc::{self, UnboundedSender};

use understudy_core::chat::system_with_activity;
use understudy_core::config::load_config;
use understudy_core::context::event_line;
use understudy_core::events::{Event, EventKind};
use understudy_core::filters::ThinkFilter;
use understudy_core::models::{build_provider, ChatMessage, Provider};
use understudy_core::sources::claude_code::{discover_sessions, ClaudeCodeSource, SessionInfo};
use understudy_core::store::EventStore;
use understudy_core::summary::summary_line;

enum Mode {
    Picker,
    Feed,
}

#[derive(PartialEq)]
enum Role {
    User,
    Bot,
    System,
}

struct ChatEntry {
    role: Role,
    text: String,
    streaming: bool,
}

impl ChatEntry {
    fn user(text: String) -> Self {
        ChatEntry { role: Role::User, text, streaming: false }
    }
    fn bot_streaming() -> Self {
        ChatEntry { role: Role::Bot, text: String::new(), streaming: true }
    }
    fn system(text: String) -> Self {
        ChatEntry { role: Role::System, text, streaming: false }
    }
}

/// Messages from the streaming chat task back to the UI loop.
enum ChatMsg {
    Delta(String),
    Done,
    Error(String),
}

pub struct App {
    mode: Mode,
    sessions: Vec<SessionInfo>,
    picker: ListState,
    store: EventStore,
    title: String,
    scroll: u16, // feed lines scrolled up from the bottom (0 = following)
    provider: Option<Provider>,
    chat_open: bool,
    chat_input: String,
    chat_log: Vec<ChatEntry>,
    chat_streaming: bool,
    should_quit: bool,
    tailer: Option<tokio::task::JoinHandle<()>>,
}

impl App {
    fn new() -> Self {
        let sessions = discover_sessions(None);
        let mut picker = ListState::default();
        if !sessions.is_empty() {
            picker.select(Some(0));
        }
        App {
            mode: Mode::Picker,
            sessions,
            picker,
            store: EventStore::new(),
            title: String::new(),
            scroll: 0,
            provider: build_provider(&load_config().provider),
            chat_open: false,
            chat_input: String::new(),
            chat_log: Vec::new(),
            chat_streaming: false,
            should_quit: false,
            tailer: None,
        }
    }

    fn attach(&mut self, tx: &UnboundedSender<Vec<Event>>) {
        let Some(info) = self.picker.selected().and_then(|i| self.sessions.get(i)) else {
            return;
        };
        let path = info.path.clone();
        self.title = short_project(&info.cwd);
        self.store = EventStore::new();
        self.scroll = 0;
        self.chat_log.clear();
        self.mode = Mode::Feed;
        if let Some(handle) = self.tailer.take() {
            handle.abort();
        }
        let tx = tx.clone();
        self.tailer = Some(tokio::spawn(async move {
            let mut src = ClaudeCodeSource::new(&path);
            let _ = tx.send(src.backfill());
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let events = src.read_new();
                if !events.is_empty() && tx.send(events).is_err() {
                    break;
                }
            }
        }));
    }

    fn back_to_picker(&mut self) {
        if let Some(handle) = self.tailer.take() {
            handle.abort();
        }
        self.chat_open = false;
        self.mode = Mode::Picker;
    }

    fn send_chat(&mut self, chat_tx: &UnboundedSender<ChatMsg>) {
        let question = self.chat_input.trim().to_string();
        if question.is_empty() || self.chat_streaming {
            return;
        }
        self.chat_input.clear();
        if self.provider.is_none() {
            self.chat_log.push(ChatEntry::system("No model configured — set one up to chat.".into()));
            return;
        }
        self.chat_log.push(ChatEntry::user(question));
        let messages = self.build_messages();
        self.chat_log.push(ChatEntry::bot_streaming());
        self.chat_streaming = true;

        let stream = self.provider.as_ref().unwrap().stream_chat(messages);
        let tx = chat_tx.clone();
        tokio::spawn(async move {
            let mut filter = ThinkFilter::new();
            let mut stream = stream;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(delta) => {
                        let visible = filter.feed(&delta);
                        if !visible.is_empty() {
                            let _ = tx.send(ChatMsg::Delta(visible));
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(ChatMsg::Error(e.to_string()));
                        return;
                    }
                }
            }
            let tail = filter.flush();
            if !tail.is_empty() {
                let _ = tx.send(ChatMsg::Delta(tail));
            }
            let _ = tx.send(ChatMsg::Done);
        });
    }

    fn build_messages(&self) -> Vec<ChatMessage> {
        let mut msgs = vec![system_with_activity(&self.store)];
        for e in &self.chat_log {
            match e.role {
                Role::User => msgs.push(ChatMessage::user(e.text.clone())),
                Role::Bot if !e.streaming => msgs.push(ChatMessage::assistant(e.text.clone())),
                _ => {}
            }
        }
        msgs
    }

    fn on_chat_msg(&mut self, msg: ChatMsg) {
        match msg {
            ChatMsg::Delta(d) => {
                if let Some(last) = self.chat_log.last_mut() {
                    if last.role == Role::Bot {
                        last.text.push_str(&d);
                    }
                }
            }
            ChatMsg::Done => {
                if let Some(last) = self.chat_log.last_mut() {
                    last.streaming = false;
                }
                self.chat_streaming = false;
            }
            ChatMsg::Error(e) => {
                if let Some(last) = self.chat_log.last_mut() {
                    last.text = format!("error: {e}");
                    last.streaming = false;
                }
                self.chat_streaming = false;
            }
        }
    }
}

pub async fn run() -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal).await;
    ratatui::restore();
    result
}

async fn event_loop(terminal: &mut DefaultTerminal) -> Result<()> {
    let mut app = App::new();
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<Vec<Event>>();
    let (chat_tx, mut chat_rx) = mpsc::unbounded_channel::<ChatMsg>();
    let mut reader = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(200));

    loop {
        terminal.draw(|f| ui(f, &mut app))?;
        if app.should_quit {
            return Ok(());
        }
        tokio::select! {
            maybe = reader.next() => {
                if let Some(Ok(CEvent::Key(key))) = maybe {
                    if key.kind == KeyEventKind::Press {
                        handle_key(&mut app, key.code, key.modifiers, &ev_tx, &chat_tx);
                    }
                }
            }
            Some(events) = ev_rx.recv() => {
                for e in events { app.store.add(e); }
            }
            Some(msg) = chat_rx.recv() => app.on_chat_msg(msg),
            _ = tick.tick() => {}
        }
    }
}

fn handle_key(
    app: &mut App,
    code: KeyCode,
    mods: KeyModifiers,
    ev_tx: &UnboundedSender<Vec<Event>>,
    chat_tx: &UnboundedSender<ChatMsg>,
) {
    // Global quit (works even while typing in the chat input)
    if matches!(code, KeyCode::Char('c') | KeyCode::Char('q')) && mods.contains(KeyModifiers::CONTROL) {
        app.should_quit = true;
        return;
    }

    // Chat input captures keys while open.
    if matches!(app.mode, Mode::Feed) && app.chat_open {
        match code {
            KeyCode::Esc => app.chat_open = false,
            KeyCode::Enter => app.send_chat(chat_tx),
            KeyCode::Backspace => {
                app.chat_input.pop();
            }
            KeyCode::Char(c) => app.chat_input.push(c),
            _ => {}
        }
        return;
    }

    match app.mode {
        Mode::Picker => match code {
            KeyCode::Char('q') => app.should_quit = true,
            KeyCode::Up | KeyCode::Char('k') => move_picker(app, -1),
            KeyCode::Down | KeyCode::Char('j') => move_picker(app, 1),
            KeyCode::Enter => app.attach(ev_tx),
            _ => {}
        },
        Mode::Feed => match code {
            KeyCode::Char('q') => app.should_quit = true,
            KeyCode::Char('c') | KeyCode::Char('/') => app.chat_open = true,
            KeyCode::Esc => app.back_to_picker(),
            KeyCode::Up | KeyCode::Char('k') => app.scroll = app.scroll.saturating_add(1),
            KeyCode::Down | KeyCode::Char('j') => app.scroll = app.scroll.saturating_sub(1),
            KeyCode::PageUp => app.scroll = app.scroll.saturating_add(10),
            KeyCode::PageDown => app.scroll = app.scroll.saturating_sub(10),
            KeyCode::Char('g') => app.scroll = u16::MAX,
            KeyCode::Char('G') => app.scroll = 0,
            _ => {}
        },
    }
}

fn move_picker(app: &mut App, delta: i32) {
    if app.sessions.is_empty() {
        return;
    }
    let n = app.sessions.len() as i32;
    let cur = app.picker.selected().unwrap_or(0) as i32;
    app.picker.select(Some((cur + delta).rem_euclid(n) as usize));
}

fn ui(f: &mut Frame, app: &mut App) {
    match app.mode {
        Mode::Picker => draw_picker(f, app),
        Mode::Feed => draw_feed(f, app),
    }
}

fn draw_picker(f: &mut Frame, app: &mut App) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(f.area());
    let items: Vec<ListItem> = app
        .sessions
        .iter()
        .map(|s| {
            let project = short_project(&s.cwd);
            let branch = if s.git_branch.is_empty() { "—" } else { &s.git_branch };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{project:<26.26} "), Style::default().fg(Color::Cyan)),
                Span::styled(format!("{branch:<14.14} "), Style::default().fg(Color::DarkGray)),
                Span::raw(truncate(&s.summary, 60)),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Understudy — select a session (read-only) "))
        .highlight_style(Style::default().bg(Color::Blue).add_modifier(Modifier::BOLD))
        .highlight_symbol("▌");
    f.render_stateful_widget(list, chunks[0], &mut app.picker);
    f.render_widget(
        Paragraph::new(" ↑↓ select · enter attach · ^q quit").style(Style::default().fg(Color::DarkGray)),
        chunks[1],
    );
}

fn draw_feed(f: &mut Frame, app: &mut App) {
    let outer = Layout::vertical([Constraint::Length(4), Constraint::Min(0), Constraint::Length(1)]).split(f.area());

    let summary = Paragraph::new(summary_line(&app.store))
        .block(Block::default().borders(Borders::ALL).title(format!(" ▶ {} ", project_title(app))))
        .wrap(Wrap { trim: true });
    f.render_widget(summary, outer[0]);

    if app.chat_open {
        let cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(outer[1]);
        draw_activity(f, app, cols[0]);
        draw_chat(f, app, cols[1]);
    } else {
        draw_activity(f, app, outer[1]);
    }

    let hint = if app.chat_open {
        " type to ask · enter send · esc close chat · ^q quit"
    } else {
        " ↑↓/PgUp/PgDn scroll · g/G top/bottom · c chat · esc sessions · ^q quit"
    };
    f.render_widget(Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)), outer[2]);
}

fn draw_activity(f: &mut Frame, app: &App, area: Rect) {
    let height = area.height.saturating_sub(2) as usize; // minus borders
    let total = app.store.events.len();
    let max_scroll = total.saturating_sub(height) as u16;
    let scroll = app.scroll.min(max_scroll);
    let end = total.saturating_sub(scroll as usize);
    let start = end.saturating_sub(height);
    let items: Vec<ListItem> = app.store.events[start..end].iter().map(event_item).collect();
    f.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title(" Activity ")),
        area,
    );
}

fn draw_chat(f: &mut Frame, app: &App, area: Rect) {
    let parts = Layout::vertical([Constraint::Min(0), Constraint::Length(3)]).split(area);
    let transcript = parts[0];
    let input = parts[1];

    let inner_w = transcript.width.saturating_sub(2);
    let inner_h = transcript.height.saturating_sub(2) as usize;
    let lines = chat_lines(app, inner_w);
    let start = lines.len().saturating_sub(inner_h);
    let title = if app.chat_streaming { " Chat — thinking… " } else { " Chat " };
    f.render_widget(
        Paragraph::new(lines[start..].to_vec()).block(Block::default().borders(Borders::ALL).title(title)),
        transcript,
    );

    let prompt = format!("> {}", app.chat_input);
    f.render_widget(
        Paragraph::new(prompt.clone()).block(Block::default().borders(Borders::ALL).title(" ask ")),
        input,
    );
    // Blinking cursor at the end of the input text.
    let cursor_x = input.x + 1 + 2 + app.chat_input.chars().count() as u16;
    let max_x = input.x + input.width.saturating_sub(2);
    f.set_cursor_position(Position::new(cursor_x.min(max_x), input.y + 1));
}

fn chat_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for e in &app.chat_log {
        let (label, color) = match e.role {
            Role::User => ("you", Color::Cyan),
            Role::Bot => ("understudy", Color::Green),
            Role::System => ("·", Color::Yellow),
        };
        lines.push(Line::styled(label.to_string(), Style::default().fg(color).add_modifier(Modifier::BOLD)));
        let body = if e.text.is_empty() && e.streaming { "…".to_string() } else { e.text.clone() };
        for w in wrap(&body, width as usize) {
            lines.push(Line::raw(w));
        }
        lines.push(Line::raw(""));
    }
    lines
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out = Vec::new();
    for para in text.split('\n') {
        if para.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        for word in para.split(' ') {
            if line.is_empty() {
                line = word.to_string();
            } else if line.chars().count() + 1 + word.chars().count() <= width {
                line.push(' ');
                line.push_str(word);
            } else {
                out.push(std::mem::take(&mut line));
                line = word.to_string();
            }
            while line.chars().count() > width {
                let head: String = line.chars().take(width).collect();
                out.push(head);
                line = line.chars().skip(width).collect();
            }
        }
        if !line.is_empty() {
            out.push(line);
        }
    }
    out
}

fn project_title(app: &App) -> String {
    if app.title.is_empty() {
        "session".to_string()
    } else {
        app.title.clone()
    }
}

fn event_item(ev: &Event) -> ListItem<'static> {
    let (icon, color) = match &ev.kind {
        EventKind::UserPrompt { .. } => ("▷", Color::Cyan),
        EventKind::AssistantText { .. } => ("✎", Color::White),
        EventKind::Thinking { .. } => ("✲", Color::Magenta),
        EventKind::ToolCall { .. } => ("→", Color::Yellow),
        EventKind::ToolResult { ok, .. } => {
            if *ok {
                ("←", Color::Green)
            } else {
                ("✗", Color::Red)
            }
        }
        EventKind::FileEdit { .. } => ("±", Color::Blue),
        EventKind::SessionStart { .. } => ("●", Color::DarkGray),
        _ => ("•", Color::Gray),
    };
    ListItem::new(Line::from(vec![
        Span::styled(format!("{icon} "), Style::default().fg(color)),
        Span::raw(event_line(ev)),
    ]))
}

fn short_project(cwd: &str) -> String {
    if cwd.is_empty() {
        return String::new();
    }
    let parts: Vec<&str> = cwd.rsplit('/').take(2).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::path::PathBuf;
    use understudy_core::sources::claude_code::ClaudeCodeSource;

    fn fixture_store() -> EventStore {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/sample_session.jsonl");
        let mut store = EventStore::new();
        store.bulk_add(ClaudeCodeSource::new(path).backfill());
        store
    }

    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn feed_renders_summary_and_events() {
        let mut app = App::new();
        app.mode = Mode::Feed;
        app.store = fixture_store();
        app.title = "proj".into();

        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("Activity"));
        assert!(text.contains("config.json"));
    }

    #[test]
    fn picker_renders_without_panic() {
        let mut app = App::new();
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        assert!(buffer_text(terminal.backend().buffer()).contains("select a session"));
    }

    #[test]
    fn chat_panel_streams_into_transcript() {
        let mut app = App::new();
        app.mode = Mode::Feed;
        app.store = fixture_store();
        app.chat_open = true;

        // Simulate a question + streamed answer without a real provider.
        app.chat_log.push(ChatEntry::user("why edit config.json?".into()));
        app.chat_log.push(ChatEntry::bot_streaming());
        app.chat_streaming = true;
        app.on_chat_msg(ChatMsg::Delta("It renamed ".into()));
        app.on_chat_msg(ChatMsg::Delta("the title.".into()));
        app.on_chat_msg(ChatMsg::Done);
        assert!(!app.chat_streaming);

        let mut terminal = Terminal::new(TestBackend::new(100, 22)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("Chat"));
        assert!(text.contains("why edit config.json?"));
        assert!(text.contains("It renamed the title."));
    }

    #[test]
    fn build_messages_includes_history_and_activity() {
        let mut app = App::new();
        app.store = fixture_store();
        app.chat_log.push(ChatEntry::user("q1".into()));
        app.chat_log.push(ChatEntry { role: Role::Bot, text: "a1".into(), streaming: false });
        let msgs = app.build_messages();
        assert_eq!(msgs[0].role, "system");
        assert!(msgs[0].content.contains("AGENT ACTIVITY"));
        assert!(msgs.iter().any(|m| m.role == "user" && m.content == "q1"));
        assert!(msgs.iter().any(|m| m.role == "assistant" && m.content == "a1"));
    }
}
