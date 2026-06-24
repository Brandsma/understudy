//! ratatui TUI: a chat-first comprehension cockpit. A persistent chat spine plus live
//! panels (Glance / Activity / Thinking / Detail / Segments) render the observed agent's
//! state on one screen. The session picker is the launch screen; attaching opens the cockpit.

use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event as CEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::layout::Position;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::DefaultTerminal;
use tokio::sync::mpsc::{self, UnboundedSender};

use understudy_core::chat::system_with_activity;
use understudy_core::comprehension::{
    coverage, explain_request, grade_request, parse_tags, parse_verdict, tag_request, Band,
    CoverageReport, Interactions, SegmentState, Verdict,
};
use understudy_core::config::load_config;
use understudy_core::context::{clip, event_line};
use understudy_core::events::{Event, EventKind, Hunk};
use understudy_core::filters::{strip_think, ThinkFilter};
use understudy_core::models::{build_provider, ChatMessage, Provider};
use understudy_core::segments::{build_segments, parse_boundaries, segment_request, Segment};
use understudy_core::sources::claude_code::{discover_sessions, ClaudeCodeSource, SessionInfo};
use understudy_core::store::EventStore;
use understudy_core::summary::live_summary_messages;

/// Height (rows) reserved for the chat spine at the bottom of the cockpit.
const CHAT_H: u16 = 9;
/// Minimum body width/height to render the full three-column cockpit (else stacked).
const WIDE_COLS: u16 = 100;
const WIDE_ROWS: u16 = 12;
/// Quiet period after the last event before the Tier-2 summary is (re)computed.
const SUMMARY_DEBOUNCE: Duration = Duration::from_millis(1500);
/// Chat `/help` text.
const HELP: &str = "commands: /segments  /debt  /explain [n]  /tagging  /follow  /session  /model [name]  \
/clear  /help  ·  tab: focus a panel  ·  ↑↓: select  ·  esc: unpin → back";

enum Mode {
    Picker,
    Cockpit,
}

/// Panels that `Tab` can make active for scrolling/selection. Chat is always live and
/// is not part of this cycle; Glance/Detail are passive readouts.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Panel {
    Activity,
    Segments,
    Thinking,
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

/// Result of a debounced Tier-2 summary task. Errors are non-fatal — the prior summary
/// stays on screen — so they carry no payload.
enum SummaryMsg {
    Done(String),
    Error,
}

/// Result of an on-demand `/segments` run: the raw model reply (parsed on the UI thread,
/// where the store lives) or an error message.
enum SegMsg {
    Done(String),
    Error(String),
}

/// Raw reply from a Tier-2 question-tagging task (parsed + applied on the UI thread).
enum TagMsg {
    Done(String),
}

/// Replies from the explain-back flow (raw model text, parsed on the UI thread).
enum ExplainMsg {
    Question { seg: usize, raw: String },
    Verdict { seg: usize, raw: String },
}

/// An in-progress explain-back: the segment and the question the model posed.
struct Explain {
    seg: usize,
    question: String,
}

/// The senders the key handler routes work onto (bundled to keep signatures small).
struct Channels {
    ev: UnboundedSender<Vec<Event>>,
    chat: UnboundedSender<ChatMsg>,
    seg: UnboundedSender<SegMsg>,
    tag: UnboundedSender<TagMsg>,
    explain: UnboundedSender<ExplainMsg>,
}

pub struct App {
    mode: Mode,
    sessions: Vec<SessionInfo>,
    picker: ListState,
    store: EventStore,
    title: String,
    branch: String,
    scroll: u16, // activity lines scrolled up from the bottom (0 = following)
    activity_sel: Option<usize>, // selected event index while Activity is focused
    active: Option<Panel>,
    provider: Option<Provider>,
    segments: Vec<Segment>,
    segments_sel: Option<usize>,
    segments_loading: bool,
    seg_map: Vec<usize>, // listing-line → event-index map for the in-flight request
    interactions: Interactions, // comprehension signals (seen / inquiry / overrides)
    tagging_enabled: bool,      // Tier-2: LLM-tag each question (opt-in, costs a call)
    awaiting_explain: Option<Explain>, // explain-back: next chat turn is the answer
    glance_summary: String, // Tier-2 "what & why" (debounced)
    summary_loading: bool,
    summary_dirty: bool, // events changed since the last summary
    last_event_at: Instant,
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
            branch: String::new(),
            scroll: 0,
            activity_sel: None,
            active: None,
            provider: build_provider(&load_config().provider),
            segments: Vec::new(),
            segments_sel: None,
            segments_loading: false,
            seg_map: Vec::new(),
            interactions: Interactions::new(),
            tagging_enabled: false,
            awaiting_explain: None,
            glance_summary: String::new(),
            summary_loading: false,
            summary_dirty: false,
            last_event_at: Instant::now(),
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
        self.branch = info.git_branch.clone();
        self.store = EventStore::new();
        self.scroll = 0;
        self.activity_sel = None;
        self.active = None;
        self.segments.clear();
        self.segments_sel = None;
        self.segments_loading = false;
        self.seg_map.clear();
        self.interactions = Interactions::new();
        self.awaiting_explain = None;
        self.glance_summary.clear();
        self.summary_loading = false;
        self.summary_dirty = false;
        self.chat_log.clear();
        self.mode = Mode::Cockpit;
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
        self.active = None;
        self.mode = Mode::Picker;
    }

    /// Cycle the active (Tab-focusable) panel forward (`dir > 0`) or backward.
    fn cycle_panel(&mut self, dir: i32) {
        let order = [Panel::Activity, Panel::Segments, Panel::Thinking];
        let cur = self.active.and_then(|p| order.iter().position(|&q| q == p));
        let next = match cur {
            // None → first (fwd) or last (back)
            None => {
                if dir > 0 {
                    Some(0)
                } else {
                    Some(order.len() - 1)
                }
            }
            Some(i) => {
                let n = order.len() as i32;
                let j = i as i32 + dir;
                if j < 0 || j >= n {
                    None // wrap back to "no active panel" (pure chat)
                } else {
                    Some(j as usize)
                }
            }
        };
        self.active = next.map(|i| order[i]);
    }

    /// Route `↑↓`/`PgUp`/`PgDn` to whichever panel is focused.
    fn nav_active(&mut self, delta: i32) {
        match self.active {
            Some(Panel::Activity) => self.nav_event(delta),
            Some(Panel::Segments) => self.nav_segment(delta),
            _ => {}
        }
    }

    /// Move the pinned event (older with `delta < 0`, newer with `delta > 0`); the first
    /// move starts at the latest event. Pinning drives the feed window and Detail.
    fn nav_event(&mut self, delta: i32) {
        let total = self.store.events.len();
        if total == 0 {
            return;
        }
        let cur = self.activity_sel.map(|i| i as i32).unwrap_or(total as i32 - 1);
        let next = (cur + delta).clamp(0, total as i32 - 1) as usize;
        self.activity_sel = Some(next);
        self.interactions.mark_seen(next); // selecting a row shows it in Detail = read
    }

    /// Move the segment selection and jump the feed to that segment's first event.
    fn nav_segment(&mut self, delta: i32) {
        if self.segments.is_empty() {
            return;
        }
        let n = self.segments.len() as i32;
        // First press selects the first segment; subsequent presses move from there.
        let next = match self.segments_sel {
            None => 0,
            Some(i) => (i as i32 + delta).clamp(0, n - 1) as usize,
        };
        self.segments_sel = Some(next);
        let start = self.segments[next].start_idx;
        self.activity_sel = Some(start);
        self.interactions.mark_seen(start); // jumping to a segment counts as skimming it
    }

    /// Build the `/segments` request and run the LLM call as a detached stream. Segments
    /// are assembled on the UI thread once the reply arrives (see [`Self::on_seg_msg`]).
    fn start_segmentation(&mut self, tx: &UnboundedSender<SegMsg>) {
        if self.segments_loading {
            return;
        }
        if self.provider.is_none() {
            self.chat_log.push(ChatEntry::system("No model configured — can't segment.".into()));
            return;
        }
        let (messages, map) = segment_request(&self.store);
        if map.is_empty() {
            self.chat_log.push(ChatEntry::system("No activity to segment yet.".into()));
            return;
        }
        self.seg_map = map;
        self.segments_loading = true;
        let stream = self.provider.as_ref().unwrap().stream_chat(messages);
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut stream = stream;
            let mut out = String::new();
            let mut err = None;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(d) => out.push_str(&d),
                    Err(e) => {
                        err = Some(e.to_string());
                        break;
                    }
                }
            }
            let _ = match err {
                Some(e) => tx.send(SegMsg::Error(e)),
                None => tx.send(SegMsg::Done(out)),
            };
        });
    }

    fn on_seg_msg(&mut self, msg: SegMsg) {
        self.segments_loading = false;
        match msg {
            SegMsg::Done(raw) => {
                self.segments = build_segments(parse_boundaries(&raw), &self.seg_map, &self.store);
                self.segments_sel = None;
            }
            SegMsg::Error(e) => {
                self.chat_log.push(ChatEntry::system(format!("Segmentation failed: {e}")));
            }
        }
    }

    /// Dispatch an Enter press: slash commands run locally, everything else is a chat turn.
    fn submit(
        &mut self,
        chat_tx: &UnboundedSender<ChatMsg>,
        seg_tx: &UnboundedSender<SegMsg>,
        tag_tx: &UnboundedSender<TagMsg>,
        explain_tx: &UnboundedSender<ExplainMsg>,
    ) {
        let input = self.chat_input.trim().to_string();
        if input.is_empty() {
            return;
        }
        if !input.starts_with('/') {
            // While an explain-back is open, the next message is the answer to grade.
            if let Some(ex) = self.awaiting_explain.take() {
                self.chat_input.clear();
                self.chat_log.push(ChatEntry::user(input.clone()));
                self.start_grade(ex.seg, &ex.question, &input, explain_tx);
                return;
            }
            // Attribute the question to segment(s): Tier-2 LLM tagging if enabled, else the
            // Tier-1 pin-at-ask-time heuristic.
            if self.tagging_enabled {
                self.start_tagging(&input, tag_tx);
            } else if let Some(idx) = self.activity_sel {
                self.interactions.mark_inquiry(idx);
            }
            self.send_chat(chat_tx);
            return;
        }
        self.chat_input.clear();
        let mut parts = input.splitn(2, char::is_whitespace);
        let cmd = parts.next().unwrap_or("");
        let arg = parts.next().unwrap_or("").trim();
        match cmd {
            "/segments" => self.start_segmentation(seg_tx),
            "/debt" => self.cmd_debt(),
            "/explain" => self.start_explain(arg, explain_tx),
            "/tagging" => self.cmd_tagging(),
            "/session" => self.back_to_picker(),
            "/follow" => {
                self.activity_sel = None;
                self.segments_sel = None;
            }
            "/model" => self.cmd_model(arg),
            "/clear" => self.chat_log.clear(),
            "/help" => self.chat_log.push(ChatEntry::system(HELP.into())),
            other => self
                .chat_log
                .push(ChatEntry::system(format!("unknown command: {other} (try /help)"))),
        }
    }

    /// Toggle Tier-2 question tagging.
    fn cmd_tagging(&mut self) {
        self.tagging_enabled = !self.tagging_enabled;
        let state = if self.tagging_enabled {
            "on — questions are LLM-classified (inquiry vs delegation) per ask"
        } else {
            "off — using the pin-at-ask-time heuristic"
        };
        self.chat_log.push(ChatEntry::system(format!("question tagging {state}")));
    }

    /// Run Tier-2 tagging for `question` as a detached stream; applied in `on_tag_msg`.
    fn start_tagging(&self, question: &str, tx: &UnboundedSender<TagMsg>) {
        let Some(provider) = self.provider.as_ref() else {
            return;
        };
        if self.segments.is_empty() {
            return; // nothing to attribute to
        }
        let stream = provider.stream_chat(tag_request(&self.segments, question));
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut stream = stream;
            let mut out = String::new();
            while let Some(item) = stream.next().await {
                match item {
                    Ok(d) => out.push_str(&d),
                    Err(_) => return, // tagging is best-effort; drop on error
                }
            }
            let _ = tx.send(TagMsg::Done(out));
        });
    }

    fn on_tag_msg(&mut self, msg: TagMsg) {
        let TagMsg::Done(raw) = msg;
        if let Some(tags) = parse_tags(&raw) {
            self.interactions.apply_tags(&tags, self.segments.len());
        }
    }

    /// `/explain [n]` — quiz yourself on a segment (defaults to the least-understood one).
    /// The model poses a "why" question; your next message is graded against the activity.
    fn start_explain(&mut self, arg: &str, tx: &UnboundedSender<ExplainMsg>) {
        let Some(provider) = self.provider.as_ref() else {
            self.chat_log.push(ChatEntry::system("No model configured — can't run explain-back.".into()));
            return;
        };
        if self.segments.is_empty() {
            self.chat_log.push(ChatEntry::system("No segments yet — run /segments first.".into()));
            return;
        }
        let seg = if arg.is_empty() {
            self.least_understood_segment()
        } else {
            arg.parse::<usize>().ok().map(|n| n.saturating_sub(1)).filter(|&i| i < self.segments.len())
        };
        let Some(seg) = seg else {
            self.chat_log.push(ChatEntry::system("Nothing left to explain — all segments understood.".into()));
            return;
        };
        let stream = provider.stream_chat(explain_request(&self.store, &self.segments[seg]));
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut stream = stream;
            let mut out = String::new();
            while let Some(item) = stream.next().await {
                match item {
                    Ok(d) => out.push_str(&d),
                    Err(_) => return,
                }
            }
            let _ = tx.send(ExplainMsg::Question { seg, raw: out });
        });
    }

    fn start_grade(&self, seg: usize, question: &str, answer: &str, tx: &UnboundedSender<ExplainMsg>) {
        let Some(provider) = self.provider.as_ref() else {
            return;
        };
        let Some(segment) = self.segments.get(seg) else {
            return;
        };
        let stream = provider.stream_chat(grade_request(&self.store, segment, question, answer));
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut stream = stream;
            let mut out = String::new();
            while let Some(item) = stream.next().await {
                match item {
                    Ok(d) => out.push_str(&d),
                    Err(_) => return,
                }
            }
            let _ = tx.send(ExplainMsg::Verdict { seg, raw: out });
        });
    }

    fn on_explain_msg(&mut self, msg: ExplainMsg) {
        match msg {
            ExplainMsg::Question { seg, raw } => {
                let q = strip_think(&raw).trim().to_string();
                if q.is_empty() {
                    self.chat_log.push(ChatEntry::system("Couldn't generate an explain-back question.".into()));
                    return;
                }
                self.chat_log.push(ChatEntry::system(format!("explain-back · segment {}: {q}", seg + 1)));
                self.awaiting_explain = Some(Explain { seg, question: q });
            }
            ExplainMsg::Verdict { seg, raw } => {
                let (verdict, note) = parse_verdict(&raw).unwrap_or((Verdict::Partial, String::new()));
                let label = match verdict {
                    Verdict::Pass => {
                        self.interactions.set_override(seg, SegmentState::Understood);
                        "pass ✓"
                    }
                    Verdict::Partial => {
                        self.interactions.set_override(seg, SegmentState::Skimmed);
                        "partial"
                    }
                    Verdict::Fail => "fail ✗",
                };
                let suffix = if note.is_empty() { String::new() } else { format!(" — {note}") };
                self.chat_log.push(ChatEntry::system(format!("explain-back {label}{suffix}")));
            }
        }
    }

    /// The first segment that isn't yet Understood (the most useful to quiz on).
    fn least_understood_segment(&self) -> Option<usize> {
        let report = coverage(&self.segments, &self.interactions);
        report.per_segment.iter().position(|&s| s != SegmentState::Understood)
    }

    /// Print the current Comprehension Coverage breakdown into the chat.
    fn cmd_debt(&mut self) {
        if self.segments.is_empty() {
            self.chat_log.push(ChatEntry::system("No segments yet — run /segments first.".into()));
            return;
        }
        let r = coverage(&self.segments, &self.interactions);
        let msg = format!(
            "comprehension {}% (est.) · {} of {} segments unreviewed · {} unread lines",
            r.percent(),
            r.unreviewed_segments,
            self.segments.len(),
            r.unread_lines
        );
        self.chat_log.push(ChatEntry::system(msg));
    }

    /// `/model` reports the current model; `/model <name>` switches it live (same
    /// provider kind/endpoint), without editing the config file.
    fn cmd_model(&mut self, arg: &str) {
        if arg.is_empty() {
            let msg = match &self.provider {
                Some(p) => format!("model: {}/{}", p.kind(), p.model()),
                None => "no model configured".to_string(),
            };
            self.chat_log.push(ChatEntry::system(msg));
            return;
        }
        let mut cfg = load_config();
        cfg.provider.model = arg.to_string();
        match build_provider(&cfg.provider) {
            Some(p) => {
                let msg = format!("switched model → {}/{}", p.kind(), p.model());
                self.provider = Some(p);
                self.chat_log.push(ChatEntry::system(msg));
            }
            None => self
                .chat_log
                .push(ChatEntry::system("no provider configured — can't switch model".into())),
        }
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

    /// Spawn a Tier-2 summary if the window changed and has been quiet long enough. The
    /// request runs as a detached `'static` stream (like chat), so the UI never blocks.
    fn maybe_summarize(&mut self, tx: &UnboundedSender<SummaryMsg>) {
        if !matches!(self.mode, Mode::Cockpit) || self.summary_loading || !self.summary_dirty {
            return;
        }
        if self.store.events.is_empty() || self.last_event_at.elapsed() < SUMMARY_DEBOUNCE {
            return;
        }
        let Some(provider) = self.provider.as_ref() else {
            return;
        };
        self.summary_dirty = false;
        self.summary_loading = true;
        let stream = provider.stream_chat(live_summary_messages(&self.store));
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut stream = stream;
            let mut out = String::new();
            let mut err = None;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(d) => out.push_str(&d),
                    Err(e) => {
                        err = Some(e.to_string());
                        break;
                    }
                }
            }
            let _ = match err {
                Some(_) => tx.send(SummaryMsg::Error),
                None => tx.send(SummaryMsg::Done(strip_think(&out).trim().to_string())),
            };
        });
    }

    fn on_summary_msg(&mut self, msg: SummaryMsg) {
        self.summary_loading = false;
        if let SummaryMsg::Done(s) = msg {
            if !s.is_empty() {
                self.glance_summary = s;
            }
        }
        // On error, keep the previous summary rather than blanking the panel.
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
    let (sum_tx, mut sum_rx) = mpsc::unbounded_channel::<SummaryMsg>();
    let (seg_tx, mut seg_rx) = mpsc::unbounded_channel::<SegMsg>();
    let (tag_tx, mut tag_rx) = mpsc::unbounded_channel::<TagMsg>();
    let (explain_tx, mut explain_rx) = mpsc::unbounded_channel::<ExplainMsg>();
    let channels = Channels { ev: ev_tx, chat: chat_tx, seg: seg_tx, tag: tag_tx, explain: explain_tx };
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
                        handle_key(&mut app, key.code, key.modifiers, &channels);
                    }
                }
            }
            Some(events) = ev_rx.recv() => {
                if !events.is_empty() {
                    for e in events { app.store.add(e); }
                    app.summary_dirty = true;
                    app.last_event_at = Instant::now();
                }
            }
            Some(msg) = chat_rx.recv() => app.on_chat_msg(msg),
            Some(msg) = sum_rx.recv() => app.on_summary_msg(msg),
            Some(msg) = seg_rx.recv() => app.on_seg_msg(msg),
            Some(msg) = tag_rx.recv() => app.on_tag_msg(msg),
            Some(msg) = explain_rx.recv() => app.on_explain_msg(msg),
            _ = tick.tick() => {}
        }
        app.maybe_summarize(&sum_tx);
    }
}

fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers, ch: &Channels) {
    // Global quit (works even while typing in the chat input).
    if matches!(code, KeyCode::Char('c') | KeyCode::Char('q')) && mods.contains(KeyModifiers::CONTROL) {
        app.should_quit = true;
        return;
    }

    match app.mode {
        Mode::Picker => match code {
            KeyCode::Char('q') => app.should_quit = true,
            KeyCode::Up | KeyCode::Char('k') => move_picker(app, -1),
            KeyCode::Down | KeyCode::Char('j') => move_picker(app, 1),
            KeyCode::Enter => app.attach(&ch.ev),
            _ => {}
        },
        // Chat-first: typing always goes to the input; Tab cycles which panel scrolls.
        Mode::Cockpit => match code {
            KeyCode::Tab => app.cycle_panel(1),
            KeyCode::BackTab => app.cycle_panel(-1),
            // Three-stage Esc: unfocus panel → unpin selection (resume live) → back to picker.
            KeyCode::Esc => {
                if app.active.is_some() {
                    app.active = None;
                } else if app.activity_sel.is_some() {
                    app.activity_sel = None;
                    app.segments_sel = None;
                } else {
                    app.back_to_picker();
                }
            }
            KeyCode::Enter => app.submit(&ch.chat, &ch.seg, &ch.tag, &ch.explain),
            KeyCode::Backspace => {
                app.chat_input.pop();
            }
            KeyCode::Up => app.nav_active(-1),
            KeyCode::Down => app.nav_active(1),
            KeyCode::PageUp => app.nav_active(-10),
            KeyCode::PageDown => app.nav_active(10),
            KeyCode::Char(c) => app.chat_input.push(c),
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
        Mode::Cockpit => draw_cockpit(f, app),
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

fn draw_cockpit(f: &mut Frame, app: &App) {
    let v = Layout::vertical([
        Constraint::Length(1),       // status bar
        Constraint::Min(0),          // panel grid
        Constraint::Length(CHAT_H),  // chat spine
        Constraint::Length(1),       // footer
    ])
    .split(f.area());

    // Comprehension Coverage is only meaningful once segments exist.
    let report = (!app.segments.is_empty()).then(|| coverage(&app.segments, &app.interactions));

    draw_status(f, app, v[0], report.as_ref());

    let body = v[1];
    if body.width >= WIDE_COLS && body.height >= WIDE_ROWS {
        let cols = Layout::horizontal([
            Constraint::Percentage(28),
            Constraint::Min(0),
            Constraint::Percentage(32),
        ])
        .split(body);
        let left = Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).split(cols[0]);
        draw_glance(f, app, left[0]);
        draw_segments(f, app, left[1], report.as_ref());
        draw_activity(f, app, cols[1]);
        let right = Layout::vertical([Constraint::Percentage(45), Constraint::Percentage(55)]).split(cols[2]);
        draw_thinking(f, app, right[0]);
        draw_detail(f, app, right[1]);
    } else {
        // Stacked fallback: Glance + Activity, chat never sacrificed.
        let rows = Layout::vertical([Constraint::Length(6), Constraint::Min(0)]).split(body);
        draw_glance(f, app, rows[0]);
        draw_activity(f, app, rows[1]);
    }

    draw_chat_spine(f, app, v[2]);
    draw_footer(f, app, v[3]);
}

fn panel_block(title: &str, active: bool) -> Block<'static> {
    let border = if active {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(format!(" {title} "))
}

fn draw_status(f: &mut Frame, app: &App, area: Rect, report: Option<&CoverageReport>) {
    let proj = if app.title.is_empty() { "session".to_string() } else { app.title.clone() };
    let branch = if app.branch.is_empty() { String::new() } else { format!("@{}", app.branch) };
    let model = match &app.provider {
        Some(p) => format!("{}/{}", p.kind(), p.model()),
        None => "no model".to_string(),
    };
    // Pinned to history while reviewing a past event; otherwise live/idle by the tailer.
    let (dot, dot_color) = if app.activity_sel.is_some() {
        ("⏸ history", Color::Yellow)
    } else if app.tailer.is_some() {
        ("● live", Color::Green)
    } else {
        ("○ idle", Color::DarkGray)
    };
    let mut spans = vec![
        Span::styled(" Understudy ", Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(format!("{proj}{branch}"), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled(format!("  ·  {model}  ·  "), Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{} events", app.store.events.len()), Style::default().fg(Color::DarkGray)),
        Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
        Span::styled(dot, Style::default().fg(dot_color)),
    ];
    // Comprehension Coverage gauge, colored by the research bands.
    spans.push(Span::styled("  ·  ", Style::default().fg(Color::DarkGray)));
    match report {
        Some(r) => {
            let color = match r.band() {
                Band::Low => Color::Red,
                Band::Mid => Color::Yellow,
                Band::High => Color::Green,
            };
            spans.push(Span::styled(format!("comp {}% (est.)", r.percent()), Style::default().fg(color)));
        }
        None => spans.push(Span::styled("comp —  /segments", Style::default().fg(Color::DarkGray))),
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_glance(f: &mut Frame, app: &App, area: Rect) {
    let block = panel_block("Glance", false);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let s = &app.store;
    let total_tools: usize = s.tool_counts.values().sum();
    let mut lines: Vec<Line> = vec![
        Line::styled(
            format!("▶ {}", s.last_action),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Line::from(format!(
            "{} files · +{} −{} · {} tools · {} ✗",
            s.files_touched.len(),
            s.lines_added,
            s.lines_removed,
            total_tools,
            s.error_count
        )),
    ];
    if !s.tool_counts.is_empty() {
        let mut pairs: Vec<(&String, &usize)> = s.tool_counts.iter().collect();
        pairs.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        let hist = pairs
            .iter()
            .take(6)
            .map(|(n, c)| format!("{n}×{c}"))
            .collect::<Vec<_>>()
            .join("  ");
        lines.push(Line::styled(hist, Style::default().fg(Color::Yellow)));
    }
    if !s.files_touched.is_empty() {
        let mut files: Vec<&String> = s.files_touched.keys().collect();
        files.sort();
        let names = files
            .iter()
            .map(|p| basename(p).to_string())
            .collect::<Vec<_>>()
            .join("  ");
        lines.push(Line::styled(format!("files: {names}"), Style::default().fg(Color::Blue)));
    }

    // Tier-2 "what & why" (debounced LLM), or a status hint while it computes.
    lines.push(Line::raw(""));
    if !app.glance_summary.is_empty() {
        lines.push(Line::styled(
            format!("≈ {}", app.glance_summary),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::ITALIC),
        ));
    } else if app.summary_loading {
        lines.push(Line::styled("≈ summarizing…", Style::default().fg(Color::DarkGray)));
    } else if app.provider.is_none() {
        lines.push(Line::styled("≈ no model — feed only", Style::default().fg(Color::DarkGray)));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

fn draw_segments(f: &mut Frame, app: &App, area: Rect, report: Option<&CoverageReport>) {
    let block = panel_block("Segments", app.active == Some(Panel::Segments));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.segments_loading {
        f.render_widget(
            Paragraph::new("segmenting…").style(Style::default().fg(Color::Yellow)),
            inner,
        );
        return;
    }
    if app.segments.is_empty() {
        f.render_widget(
            Paragraph::new("(none yet — /segments)").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }
    let width = inner.width.saturating_sub(5) as usize;
    let items: Vec<ListItem> = app
        .segments
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let state = report.and_then(|r| r.per_segment.get(i).copied()).unwrap_or(SegmentState::Unseen);
            let line = Line::from(vec![
                Span::styled(format!("{} ", state.glyph()), Style::default().fg(state_color(state))),
                Span::styled(format!("{} ", i + 1), Style::default().fg(Color::DarkGray)),
                Span::raw(truncate(&s.title, width)),
            ]);
            let item = ListItem::new(line);
            if Some(i) == app.segments_sel {
                item.style(Style::default().bg(Color::Rgb(40, 40, 55)).add_modifier(Modifier::BOLD))
            } else {
                item
            }
        })
        .collect();
    f.render_widget(List::new(items), inner);
}

fn draw_activity(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.active == Some(Panel::Activity);
    let block = panel_block("Activity", focused);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let height = inner.height as usize;
    let total = app.store.events.len();

    // With a pinned event, center the window on it; otherwise follow the bottom (live).
    let (start, sel_row) = match app.activity_sel {
        Some(sel) => {
            let half = height / 2;
            let max_start = total.saturating_sub(height);
            let start = sel.saturating_sub(half).min(max_start);
            (start, Some(sel - start))
        }
        None => {
            let max_scroll = total.saturating_sub(height) as u16;
            let scroll = app.scroll.min(max_scroll);
            let end = total.saturating_sub(scroll as usize);
            (end.saturating_sub(height), None)
        }
    };
    let end = (start + height).min(total);

    let items: Vec<ListItem> = app.store.events[start..end]
        .iter()
        .enumerate()
        .map(|(i, ev)| {
            let item = event_item(ev);
            if Some(i) == sel_row {
                item.style(Style::default().bg(Color::Rgb(40, 40, 55)).add_modifier(Modifier::BOLD))
            } else {
                item
            }
        })
        .collect();
    f.render_widget(List::new(items), inner);
}

fn draw_thinking(f: &mut Frame, app: &App, area: Rect) {
    let block = panel_block("Thinking", app.active == Some(Panel::Thinking));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let cap = inner.height as usize;
    let mut labels: Vec<String> = Vec::new();
    for ev in app.store.events.iter().rev() {
        if let EventKind::Thinking { text, summary } = &ev.kind {
            let label = summary
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    let t = text.trim();
                    if t.is_empty() {
                        "(not exposed)".to_string()
                    } else {
                        clip(t, 60)
                    }
                });
            labels.push(label);
            if labels.len() >= cap {
                break;
            }
        }
    }
    labels.reverse();
    if labels.is_empty() {
        f.render_widget(
            Paragraph::new("(no reasoning yet)").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }
    let lines: Vec<Line> = labels
        .into_iter()
        .map(|l| {
            Line::from(vec![
                Span::styled("✲ ", Style::default().fg(Color::Magenta)),
                Span::raw(l),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

fn draw_detail(f: &mut Frame, app: &App, area: Rect) {
    let block = panel_block("Detail", false);
    let inner = block.inner(area);
    f.render_widget(block, area);

    // A selected Activity row drives Detail; otherwise fall back to the latest edit.
    let lines = match app.activity_sel.and_then(|i| app.store.events.get(i)) {
        Some(ev) => detail_lines(ev, inner.width, inner.height),
        None => latest_edit_lines(&app.store, inner.width, inner.height),
    };
    f.render_widget(Paragraph::new(lines), inner);
}

/// Context-sensitive detail for the selected event.
fn detail_lines(ev: &Event, width: u16, height: u16) -> Vec<Line<'static>> {
    let w = width as usize;
    let budget = height as usize;
    match &ev.kind {
        EventKind::FileEdit { path, hunks, .. } => diff_lines(path, hunks, width, height),
        EventKind::ToolCall { name, input, .. } => {
            let mut lines = vec![Line::styled(
                format!("→ {name}"),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )];
            let pretty = serde_json::to_string_pretty(input).unwrap_or_default();
            for l in pretty.lines().take(budget.saturating_sub(1)) {
                lines.push(Line::raw(truncate(l, w)));
            }
            lines
        }
        EventKind::ToolResult { name, ok, summary, detail, .. } => {
            let (status, color) = if *ok { ("ok", Color::Green) } else { ("ERROR", Color::Red) };
            let mut lines = vec![Line::styled(
                format!("← {name} {status}"),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )];
            for l in summary.lines().chain(detail.lines()).take(budget.saturating_sub(1)) {
                lines.push(Line::raw(truncate(l, w)));
            }
            lines
        }
        EventKind::Thinking { text, summary } => {
            let mut lines = Vec::new();
            if let Some(s) = summary.as_ref().filter(|s| !s.is_empty()) {
                lines.push(Line::styled(format!("✲ {s}"), Style::default().fg(Color::Magenta)));
            }
            let body = if text.trim().is_empty() { "(not exposed)" } else { text };
            for w2 in wrap(body, w).into_iter().take(budget.saturating_sub(lines.len())) {
                lines.push(Line::raw(w2));
            }
            lines
        }
        EventKind::UserPrompt { text } | EventKind::AssistantText { text } => {
            wrap(text, w).into_iter().take(budget).map(Line::raw).collect()
        }
        _ => vec![Line::raw(event_line(ev))],
    }
}

fn latest_edit_lines(store: &EventStore, width: u16, height: u16) -> Vec<Line<'static>> {
    match last_file_edit(store) {
        Some((path, hunks)) => diff_lines(path, hunks, width, height),
        None => vec![Line::styled("(no file changes yet)", Style::default().fg(Color::DarkGray))],
    }
}

fn diff_lines(path: &str, hunks: &[Hunk], width: u16, height: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = vec![Line::styled(
        basename(path).to_string(),
        Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
    )];
    let budget = height.saturating_sub(1) as usize;
    'outer: for h in hunks {
        for l in &h.lines {
            if lines.len() > budget {
                break 'outer;
            }
            let color = match l.chars().next() {
                Some('+') => Color::Green,
                Some('-') => Color::Red,
                _ => Color::Gray,
            };
            lines.push(Line::styled(truncate(l, width as usize), Style::default().fg(color)));
        }
    }
    lines
}

fn draw_chat_spine(f: &mut Frame, app: &App, area: Rect) {
    let title = if app.chat_streaming { "Chat — thinking…" } else { "Chat" };
    let block = panel_block(title, false);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);
    let transcript = rows[0];
    let input = rows[1];

    let lines = chat_lines(app, transcript.width);
    let start = lines.len().saturating_sub(transcript.height as usize);
    f.render_widget(Paragraph::new(lines[start..].to_vec()), transcript);

    let prompt = format!("› {}", app.chat_input);
    f.render_widget(Paragraph::new(prompt), input);
    // Caret at the end of the input ("› " is two display columns).
    let cursor_x = input.x + 2 + app.chat_input.chars().count() as u16;
    let max_x = input.x + input.width.saturating_sub(1);
    f.set_cursor_position(Position::new(cursor_x.min(max_x), input.y));
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let esc = if app.active.is_some() {
        "esc unfocus"
    } else if app.activity_sel.is_some() {
        "esc unpin"
    } else {
        "esc sessions"
    };
    let hint = format!(" tab panes · ↑↓ select · enter send · /segments · {esc} · ^q quit");
    f.render_widget(Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)), area);
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

/// The most recent file edit's path + hunks, for the Detail readout.
fn last_file_edit(store: &EventStore) -> Option<(&str, &Vec<Hunk>)> {
    store.events.iter().rev().find_map(|ev| match &ev.kind {
        EventKind::FileEdit { path, hunks, .. } => Some((path.as_str(), hunks)),
        _ => None,
    })
}

fn short_project(cwd: &str) -> String {
    if cwd.is_empty() {
        return String::new();
    }
    let parts: Vec<&str> = cwd.rsplit('/').take(2).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn state_color(state: SegmentState) -> Color {
    match state {
        SegmentState::Unseen => Color::DarkGray,
        SegmentState::Skimmed => Color::Yellow,
        SegmentState::Understood => Color::Green,
    }
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

    fn line_text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
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

    fn cockpit_app() -> App {
        let mut app = App::new();
        app.mode = Mode::Cockpit;
        app.store = fixture_store();
        app.title = "proj".into();
        app
    }

    #[test]
    fn wide_cockpit_renders_all_panels() {
        let mut app = cockpit_app();
        let mut terminal = Terminal::new(TestBackend::new(130, 40)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        for title in ["Understudy", "Glance", "Segments", "Activity", "Thinking", "Detail", "Chat"] {
            assert!(text.contains(title), "missing panel: {title}");
        }
        assert!(text.contains("config.json")); // an edited file shows in the feed/detail
    }

    #[test]
    fn narrow_cockpit_degrades_without_panic() {
        let mut app = cockpit_app();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        // Chat is never sacrificed; Glance + Activity remain.
        assert!(text.contains("Chat"));
        assert!(text.contains("Activity"));
        assert!(text.contains("Glance"));
    }

    #[test]
    fn picker_renders_without_panic() {
        let mut app = App::new();
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        assert!(buffer_text(terminal.backend().buffer()).contains("select a session"));
    }

    #[test]
    fn tab_cycles_active_panel_then_wraps_to_none() {
        let mut app = cockpit_app();
        assert!(app.active.is_none());
        app.cycle_panel(1);
        assert_eq!(app.active, Some(Panel::Activity));
        app.cycle_panel(1);
        assert_eq!(app.active, Some(Panel::Segments));
        app.cycle_panel(1);
        assert_eq!(app.active, Some(Panel::Thinking));
        app.cycle_panel(1);
        assert!(app.active.is_none()); // wraps back to pure-chat
    }

    #[test]
    fn tier2_summary_renders_in_glance() {
        let mut app = cockpit_app();
        app.glance_summary = "refactoring the event store".into();
        let mut terminal = Terminal::new(TestBackend::new(130, 40)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        assert!(buffer_text(terminal.backend().buffer()).contains("refactoring the event store"));
    }

    #[test]
    fn activity_selection_persists_across_panel_focus() {
        let mut app = cockpit_app();
        app.cycle_panel(1); // → Activity
        assert_eq!(app.active, Some(Panel::Activity));
        app.nav_active(-1);
        assert!(app.activity_sel.is_some(), "navigating should start a selection");
        app.cycle_panel(1); // → Segments: the pin survives (cleared only by Esc-unpin)
        assert!(app.activity_sel.is_some());
    }

    #[test]
    fn typing_goes_to_chat_input() {
        let mut app = cockpit_app();
        for c in "hi".chars() {
            app.chat_input.push(c);
        }
        assert_eq!(app.chat_input, "hi");
        let mut terminal = Terminal::new(TestBackend::new(130, 40)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        assert!(buffer_text(terminal.backend().buffer()).contains("› hi"));
    }

    #[test]
    fn chat_panel_streams_into_transcript() {
        let mut app = cockpit_app();
        app.chat_log.push(ChatEntry::user("why edit config.json?".into()));
        app.chat_log.push(ChatEntry::bot_streaming());
        app.chat_streaming = true;
        app.on_chat_msg(ChatMsg::Delta("It renamed ".into()));
        app.on_chat_msg(ChatMsg::Delta("the title.".into()));
        app.on_chat_msg(ChatMsg::Done);
        assert!(!app.chat_streaming);

        let mut terminal = Terminal::new(TestBackend::new(130, 40)).unwrap();
        terminal.draw(|f| ui(f, &mut app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("why edit config.json?"));
        assert!(text.contains("It renamed the title."));
    }

    fn fake_segment(title: &str, start: usize, end: usize) -> Segment {
        Segment {
            title: title.into(),
            start_idx: start,
            end_idx: end,
            files: Vec::new(),
            lines_added: 0,
            lines_removed: 0,
            tool_counts: std::collections::BTreeMap::new(),
            errors: 0,
            first_ts: None,
            last_ts: None,
        }
    }

    #[test]
    fn unknown_slash_command_reports_to_chat() {
        let mut app = cockpit_app();
        let (chat_tx, _r1) = mpsc::unbounded_channel::<ChatMsg>();
        let (seg_tx, _r2) = mpsc::unbounded_channel::<SegMsg>();
        let (tag_tx, _r3) = mpsc::unbounded_channel::<TagMsg>();
        let (explain_tx, _r4) = mpsc::unbounded_channel::<ExplainMsg>();
        app.chat_input = "/bogus".into();
        app.submit(&chat_tx, &seg_tx, &tag_tx, &explain_tx);
        assert!(app.chat_input.is_empty());
        let last = app.chat_log.last().unwrap();
        assert!(last.role == Role::System && last.text.contains("unknown command"));
    }

    #[test]
    fn comprehension_gauge_placeholder_then_percent() {
        let mut app = cockpit_app();
        let mut t = Terminal::new(TestBackend::new(130, 40)).unwrap();
        t.draw(|f| ui(f, &mut app)).unwrap();
        assert!(buffer_text(t.backend().buffer()).contains("/segments")); // placeholder, no segments

        app.segments = vec![fake_segment("a", 0, app.store.events.len())];
        let mut t2 = Terminal::new(TestBackend::new(130, 40)).unwrap();
        t2.draw(|f| ui(f, &mut app)).unwrap();
        assert!(buffer_text(t2.backend().buffer()).contains("(est.)")); // gauge with a percent
    }

    #[test]
    fn segment_glyph_reflects_state() {
        let mut app = cockpit_app();
        app.segments = vec![fake_segment("alpha", 0, 5)];
        app.interactions.mark_inquiry(2); // inside segment 0 → Understood
        let mut t = Terminal::new(TestBackend::new(130, 40)).unwrap();
        t.draw(|f| ui(f, &mut app)).unwrap();
        assert!(buffer_text(t.backend().buffer()).contains('●'));
    }

    #[test]
    fn debt_command_reports_coverage() {
        let mut app = cockpit_app();
        let (chat_tx, _r1) = mpsc::unbounded_channel::<ChatMsg>();
        let (seg_tx, _r2) = mpsc::unbounded_channel::<SegMsg>();
        let (tag_tx, _r3) = mpsc::unbounded_channel::<TagMsg>();
        let (explain_tx, _r4) = mpsc::unbounded_channel::<ExplainMsg>();
        app.segments = vec![fake_segment("a", 0, 3)];
        app.chat_input = "/debt".into();
        app.submit(&chat_tx, &seg_tx, &tag_tx, &explain_tx);
        let last = app.chat_log.last().unwrap();
        assert!(last.text.contains("comprehension") && last.text.contains("unread lines"));
    }

    #[test]
    fn pinned_question_marks_its_segment_understood() {
        let mut app = cockpit_app();
        app.provider = None; // avoid spawning a real chat task in a sync test
        let (chat_tx, _r1) = mpsc::unbounded_channel::<ChatMsg>();
        let (seg_tx, _r2) = mpsc::unbounded_channel::<SegMsg>();
        let (tag_tx, _r3) = mpsc::unbounded_channel::<TagMsg>();
        let (explain_tx, _r4) = mpsc::unbounded_channel::<ExplainMsg>();
        app.segments = vec![fake_segment("a", 0, 5)];
        app.activity_sel = Some(2); // pinned inside segment 0
        app.chat_input = "what is this doing?".into();
        app.submit(&chat_tx, &seg_tx, &tag_tx, &explain_tx);
        let r = coverage(&app.segments, &app.interactions);
        assert_eq!(r.per_segment[0], SegmentState::Understood);
    }

    #[test]
    fn tagging_command_toggles() {
        let mut app = cockpit_app();
        let (chat_tx, _r1) = mpsc::unbounded_channel::<ChatMsg>();
        let (seg_tx, _r2) = mpsc::unbounded_channel::<SegMsg>();
        let (tag_tx, _r3) = mpsc::unbounded_channel::<TagMsg>();
        let (explain_tx, _r4) = mpsc::unbounded_channel::<ExplainMsg>();
        assert!(!app.tagging_enabled);
        app.chat_input = "/tagging".into();
        app.submit(&chat_tx, &seg_tx, &tag_tx, &explain_tx);
        assert!(app.tagging_enabled);
    }

    #[test]
    fn tag_reply_upgrades_segment_via_overrides() {
        let mut app = cockpit_app();
        app.segments = vec![fake_segment("a", 0, 5), fake_segment("b", 5, 9)];
        app.on_tag_msg(TagMsg::Done("{\"segments\":[1],\"kind\":\"inquiry\"}".into()));
        let r = coverage(&app.segments, &app.interactions);
        assert_eq!(r.per_segment[1], SegmentState::Understood);
    }

    #[test]
    fn tagging_on_skips_tier1_pin_heuristic() {
        let mut app = cockpit_app();
        app.provider = None; // start_tagging + send_chat return early, no task spawned
        app.tagging_enabled = true;
        let (chat_tx, _r1) = mpsc::unbounded_channel::<ChatMsg>();
        let (seg_tx, _r2) = mpsc::unbounded_channel::<SegMsg>();
        let (tag_tx, _r3) = mpsc::unbounded_channel::<TagMsg>();
        let (explain_tx, _r4) = mpsc::unbounded_channel::<ExplainMsg>();
        app.segments = vec![fake_segment("a", 0, 5)];
        app.activity_sel = Some(2);
        app.chat_input = "please refactor this".into();
        app.submit(&chat_tx, &seg_tx, &tag_tx, &explain_tx);
        // The Tier-1 heuristic is skipped; attribution is left to the (here no-op) tagger.
        assert_eq!(coverage(&app.segments, &app.interactions).per_segment[0], SegmentState::Unseen);
    }

    #[test]
    fn explain_question_opens_awaiting_state() {
        let mut app = cockpit_app();
        app.segments = vec![fake_segment("a", 0, 5)];
        app.on_explain_msg(ExplainMsg::Question { seg: 0, raw: "Why did it do this?".into() });
        assert!(matches!(&app.awaiting_explain, Some(e) if e.seg == 0));
        assert!(app.chat_log.last().unwrap().text.contains("Why did it do this?"));
    }

    #[test]
    fn explain_pass_marks_segment_understood() {
        let mut app = cockpit_app();
        app.segments = vec![fake_segment("a", 0, 5)];
        app.on_explain_msg(ExplainMsg::Verdict { seg: 0, raw: "{\"verdict\":\"pass\",\"note\":\"correct\"}".into() });
        assert_eq!(coverage(&app.segments, &app.interactions).per_segment[0], SegmentState::Understood);
    }

    #[test]
    fn explain_answer_routes_to_grading() {
        let mut app = cockpit_app();
        app.provider = None; // start_grade returns early; no task spawned
        app.segments = vec![fake_segment("a", 0, 5)];
        app.awaiting_explain = Some(Explain { seg: 0, question: "why?".into() });
        let (chat_tx, _r1) = mpsc::unbounded_channel::<ChatMsg>();
        let (seg_tx, _r2) = mpsc::unbounded_channel::<SegMsg>();
        let (tag_tx, _r3) = mpsc::unbounded_channel::<TagMsg>();
        let (explain_tx, _r4) = mpsc::unbounded_channel::<ExplainMsg>();
        app.chat_input = "because of X".into();
        app.submit(&chat_tx, &seg_tx, &tag_tx, &explain_tx);
        assert!(app.awaiting_explain.is_none()); // consumed
        assert_eq!(app.chat_log.last().unwrap().text, "because of X"); // answer echoed, not sent as a question
    }

    #[test]
    fn least_understood_picks_first_non_understood() {
        let mut app = cockpit_app();
        app.segments = vec![fake_segment("a", 0, 3), fake_segment("b", 3, 6)];
        app.interactions.set_override(0, SegmentState::Understood);
        assert_eq!(app.least_understood_segment(), Some(1));
    }

    #[test]
    fn follow_command_unpins_the_feed() {
        let mut app = cockpit_app();
        let (chat_tx, _r1) = mpsc::unbounded_channel::<ChatMsg>();
        let (seg_tx, _r2) = mpsc::unbounded_channel::<SegMsg>();
        let (tag_tx, _r3) = mpsc::unbounded_channel::<TagMsg>();
        let (explain_tx, _r4) = mpsc::unbounded_channel::<ExplainMsg>();
        app.activity_sel = Some(3);
        app.segments_sel = Some(1);
        app.chat_input = "/follow".into();
        app.submit(&chat_tx, &seg_tx, &tag_tx, &explain_tx);
        assert!(app.activity_sel.is_none() && app.segments_sel.is_none());
    }

    #[test]
    fn session_command_returns_to_picker() {
        let mut app = cockpit_app();
        let (chat_tx, _r1) = mpsc::unbounded_channel::<ChatMsg>();
        let (seg_tx, _r2) = mpsc::unbounded_channel::<SegMsg>();
        let (tag_tx, _r3) = mpsc::unbounded_channel::<TagMsg>();
        let (explain_tx, _r4) = mpsc::unbounded_channel::<ExplainMsg>();
        app.chat_input = "/session".into();
        app.submit(&chat_tx, &seg_tx, &tag_tx, &explain_tx);
        assert!(matches!(app.mode, Mode::Picker));
    }

    #[test]
    fn model_command_reports_current_model() {
        let mut app = cockpit_app();
        let (chat_tx, _r1) = mpsc::unbounded_channel::<ChatMsg>();
        let (seg_tx, _r2) = mpsc::unbounded_channel::<SegMsg>();
        let (tag_tx, _r3) = mpsc::unbounded_channel::<TagMsg>();
        let (explain_tx, _r4) = mpsc::unbounded_channel::<ExplainMsg>();
        app.chat_input = "/model".into();
        app.submit(&chat_tx, &seg_tx, &tag_tx, &explain_tx);
        let last = app.chat_log.last().unwrap();
        assert!(last.role == Role::System && (last.text.contains("model:") || last.text.contains("no model")));
    }

    #[test]
    fn selecting_a_segment_jumps_the_feed() {
        let mut app = cockpit_app();
        app.segments = vec![fake_segment("a", 0, 3), fake_segment("b", 5, 9)];
        app.cycle_panel(1); // Activity
        app.cycle_panel(1); // Segments
        assert_eq!(app.active, Some(Panel::Segments));
        app.nav_active(1); // select first segment → pin its start
        assert_eq!(app.segments_sel, Some(0));
        assert_eq!(app.activity_sel, Some(0));
        app.nav_active(1); // second segment
        assert_eq!(app.segments_sel, Some(1));
        assert_eq!(app.activity_sel, Some(5));
    }

    #[test]
    fn segment_results_are_assembled_from_raw_reply() {
        let mut app = cockpit_app();
        app.seg_map = (0..app.store.events.len()).collect();
        app.segments_loading = true;
        app.on_seg_msg(SegMsg::Done("[{\"start\":0,\"title\":\"All\"}]".into()));
        assert!(!app.segments_loading);
        assert_eq!(app.segments.len(), 1);
        assert_eq!(app.segments[0].title, "All");
    }

    #[test]
    fn selecting_a_file_edit_shows_its_diff_in_detail() {
        let store = fixture_store();
        let edit = store
            .events
            .iter()
            .find(|e| matches!(e.kind, EventKind::FileEdit { .. }))
            .expect("fixture has a file edit");
        let lines = detail_lines(edit, 60, 20);
        assert_eq!(line_text(&lines[0]), "config.json"); // header = basename
        assert!(
            lines.iter().any(|l| line_text(l).starts_with('-') || line_text(l).starts_with('+')),
            "expected a +/- diff line in Detail"
        );
    }

    #[test]
    fn nav_event_clamps_at_the_first_event() {
        let mut app = cockpit_app();
        app.cycle_panel(1); // Activity
        app.nav_active(-100_000); // far past the start
        assert_eq!(app.activity_sel, Some(0));
    }

    #[test]
    fn three_stage_esc_unfocus_unpin_then_picker() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = cockpit_app();
        let (ev_tx, _r0) = mpsc::unbounded_channel::<Vec<Event>>();
        let (chat_tx, _r1) = mpsc::unbounded_channel::<ChatMsg>();
        let (seg_tx, _r2) = mpsc::unbounded_channel::<SegMsg>();
        let (tag_tx, _r3) = mpsc::unbounded_channel::<TagMsg>();
        let (explain_tx, _r4) = mpsc::unbounded_channel::<ExplainMsg>();
        app.active = Some(Panel::Activity);
        app.activity_sel = Some(2);

        let ch = Channels { ev: ev_tx, chat: chat_tx, seg: seg_tx, tag: tag_tx, explain: explain_tx };
        let esc = |a: &mut App| handle_key(a, KeyCode::Esc, KeyModifiers::NONE, &ch);

        esc(&mut app); // 1: unfocus panel
        assert!(app.active.is_none() && app.activity_sel.is_some());
        esc(&mut app); // 2: unpin (resume live)
        assert!(app.activity_sel.is_none());
        assert!(matches!(app.mode, Mode::Cockpit));
        esc(&mut app); // 3: back to picker
        assert!(matches!(app.mode, Mode::Picker));
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
