//! ratatui TUI: a chat-first comprehension cockpit. A persistent chat spine plus live
//! panels (Glance / Activity / Thinking / Detail / Segments) render the observed agent's
//! state on one screen. The session picker is the launch screen; attaching opens the cockpit.

use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as CEvent, EventStream, KeyCode, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use futures::StreamExt;
use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::DefaultTerminal;
use tokio::sync::mpsc::{self, UnboundedSender};

use understudy_core::chat::system_with_activity;
use understudy_core::comprehension::{
    coverage, explain_request, grade_request, ledger, parse_tags, parse_verdict, session_cache,
    tag_request, Band, CoverageReport, Interactions, SegmentState, Verdict,
};
use understudy_core::config::load_config;
use understudy_core::context::{clip, event_line};
use understudy_core::events::{Event, EventKind, Hunk};
use understudy_core::filters::{strip_think, ThinkFilter};
use understudy_core::models::{build_provider, ChatMessage, Provider};
use understudy_core::segments::{
    batch_starts, build_segments_from_starts, parse_partial_boundaries, segment_batch_request,
    Segment, BATCH_SIZE,
};
use understudy_core::sources::{discover_all, open_source, Agent, SessionInfo};
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
const HELP: &str = "commands: /segments [--force]  /debt  /explain [n]  /show thinking  /tagging  /follow  /session  /model [name]  \
/clear  /help  ·  tab: focus a panel  ·  ↑↓ / j k (10j) / g G: move  ·  esc: unpin → back";

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
/// where the store lives) or an error message. `Progress` carries the segment titles found
/// so far while the boundary array is still streaming, for live feedback.
enum SegMsg {
    Progress(Vec<String>),
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
    picker: ListState,            // selection index into the *visible* (filtered) sessions
    picker_query: String,         // fuzzy filter typed in the picker (matches agent + title)
    agent_filter: Option<Agent>,  // None = all agents; cycled with Tab
    store: EventStore,
    title: String,
    branch: String,
    session_id: String,
    scroll: u16, // activity lines scrolled up from the bottom (0 = following)
    activity_sel: Option<usize>, // selected event index while Activity is focused
    active: Option<Panel>,
    show_thinking: bool, // Thinking panel visible (top-right); when off, Detail takes the column
    provider: Option<Provider>,
    segments: Vec<Segment>,
    segments_sel: Option<usize>,
    segments_loading: bool,
    seg_map: Vec<usize>, // listing-line → event-index map for the in-flight request
    seg_progress: Vec<String>, // segment titles discovered so far while streaming
    seg_frozen: Vec<(usize, String)>, // already-segmented blocks before the in-flight batch
    seg_batch_start: usize, // event index the in-flight batch began at
    seg_batch_started_at: Option<Instant>, // when the in-flight batch's model call began
    seg_elapsed: Duration, // cumulative model time across completed batches in this run
    seg_batches_done: usize, // completed batches in this run (for the average)
    seg_watermark: usize, // events covered by the current segments (persisted; incremental base)
    auto_segment_pending: bool, // reconcile cache / segment history once backfill first arrives
    pending_cache: Option<session_cache::SessionCache>, // loaded cache, applied after backfill
    summary_len: usize,  // event count the current glance_summary covers (for persistence)
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
    history: Vec<String>,       // submitted inputs, oldest first (shell-style command history)
    history_pos: Option<usize>, // browse cursor into `history`; None = editing the live draft
    draft: String,              // in-progress input stashed while browsing history
    nav_count: String,          // pending vim count prefix (e.g. "10" before `j`), focused only
    should_quit: bool,
    tailer: Option<tokio::task::JoinHandle<()>>,
    // Mouse: the full area last drawn (for hit-testing) and the active drag-selection in
    // terminal coordinates (x, y). `copy_pending` requests a clipboard copy on the next draw.
    last_area: Rect,
    sel_anchor: Option<(u16, u16)>,
    sel_cursor: Option<(u16, u16)>,
    copy_pending: bool,
}

impl App {
    fn new() -> Self {
        let sessions = discover_all(None);
        let mut picker = ListState::default();
        if !sessions.is_empty() {
            picker.select(Some(0));
        }
        App {
            mode: Mode::Picker,
            sessions,
            picker,
            picker_query: String::new(),
            agent_filter: None,
            store: EventStore::new(),
            title: String::new(),
            branch: String::new(),
            session_id: String::new(),
            scroll: 0,
            activity_sel: None,
            active: None,
            show_thinking: false,
            provider: build_provider(&load_config().provider),
            segments: Vec::new(),
            segments_sel: None,
            segments_loading: false,
            seg_map: Vec::new(),
            seg_progress: Vec::new(),
            seg_frozen: Vec::new(),
            seg_batch_start: 0,
            seg_batch_started_at: None,
            seg_elapsed: Duration::ZERO,
            seg_batches_done: 0,
            seg_watermark: 0,
            auto_segment_pending: false,
            pending_cache: None,
            summary_len: 0,
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
            history: Vec::new(),
            history_pos: None,
            draft: String::new(),
            nav_count: String::new(),
            should_quit: false,
            tailer: None,
            last_area: Rect::default(),
            sel_anchor: None,
            sel_cursor: None,
            copy_pending: false,
        }
    }

    /// Indices into `self.sessions` that pass the active agent filter and fuzzy query, in the
    /// original (newest-first) order. Every whitespace-separated query token must appear as a
    /// case-insensitive subsequence of "<agent name> <title>".
    fn visible_indices(&self) -> Vec<usize> {
        let tokens: Vec<String> =
            self.picker_query.to_lowercase().split_whitespace().map(str::to_string).collect();
        self.sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| self.agent_filter.is_none_or(|a| s.agent == a))
            .filter(|(_, s)| {
                tokens.is_empty() || {
                    let hay = format!("{} {}", s.agent.name(), s.summary).to_lowercase();
                    tokens.iter().all(|t| is_subsequence(t, &hay))
                }
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// The session currently highlighted in the filtered list, if any.
    fn selected_session(&self) -> Option<&SessionInfo> {
        let vis = self.visible_indices();
        self.picker.selected().and_then(|i| vis.get(i)).and_then(|&idx| self.sessions.get(idx))
    }

    /// Re-anchor the selection to the top of the (re-filtered) list, or clear it if empty.
    fn reset_picker_selection(&mut self) {
        let has_rows = !self.visible_indices().is_empty();
        self.picker.select(has_rows.then_some(0));
    }

    /// Agents present in the session list, in canonical order (drives the Tab cycle so it
    /// never lands on an agent with zero sessions).
    fn present_agents(&self) -> Vec<Agent> {
        const ORDER: [Agent; 6] = [
            Agent::ClaudeCode,
            Agent::OpenCode,
            Agent::Copilot,
            Agent::Codex,
            Agent::Antigravity,
            Agent::Unknown,
        ];
        ORDER.into_iter().filter(|a| self.sessions.iter().any(|s| s.agent == *a)).collect()
    }

    /// Cycle the agent filter `All → <each present agent> → All` (`dir > 0`) or in reverse.
    fn cycle_agent_filter(&mut self, dir: i32) {
        let agents = self.present_agents();
        if agents.is_empty() {
            return;
        }
        // The cycle is `None` (All) followed by each present agent.
        let cur = match self.agent_filter {
            None => 0,
            Some(a) => agents.iter().position(|&x| x == a).map(|i| i + 1).unwrap_or(0),
        };
        let len = agents.len() as i32 + 1;
        let next = (cur as i32 + dir).rem_euclid(len) as usize;
        self.agent_filter = (next > 0).then(|| agents[next - 1]);
        self.reset_picker_selection();
    }

    fn attach(&mut self, tx: &UnboundedSender<Vec<Event>>) {
        let Some(info) = self.selected_session() else {
            return;
        };
        let info = info.clone();
        self.title = short_project(&info.cwd);
        self.branch = info.git_branch.clone();
        self.session_id = info.session_id.clone();
        self.store = EventStore::new();
        self.scroll = 0;
        self.activity_sel = None;
        self.active = None;
        self.segments.clear();
        self.segments_sel = None;
        self.segments_loading = false;
        self.seg_map.clear();
        self.seg_progress.clear();
        self.seg_frozen.clear();
        self.seg_batch_started_at = None;
        self.seg_elapsed = Duration::ZERO;
        self.seg_batches_done = 0;
        self.seg_watermark = 0;
        self.auto_segment_pending = true; // reconcile cache / segment history once backfill lands
        self.interactions = Interactions::new();
        self.awaiting_explain = None;
        self.glance_summary.clear();
        self.summary_loading = false;
        self.summary_dirty = false;
        self.summary_len = 0;
        // Restore the persisted summary now; cached segments need the store, so they're
        // rebuilt once backfill arrives (see `reconcile_on_backfill`).
        self.pending_cache = session_cache::load(&self.session_id);
        if let Some(c) = &self.pending_cache {
            self.glance_summary = c.summary.clone();
            self.summary_len = c.summary_len;
        }
        self.chat_log.clear();
        self.history.clear();
        self.history_pos = None;
        self.draft.clear();
        self.mode = Mode::Cockpit;
        if let Some(handle) = self.tailer.take() {
            handle.abort();
        }
        let tx = tx.clone();
        self.tailer = Some(tokio::spawn(async move {
            let mut src = open_source(&info);
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
        self.persist();
        if let Some(handle) = self.tailer.take() {
            handle.abort();
        }
        self.active = None;
        self.mode = Mode::Picker;
    }

    /// Append this session's comprehension snapshot to the ledger (best-effort). Only when
    /// there's something to record (cockpit with computed segments).
    fn persist(&self) {
        if !matches!(self.mode, Mode::Cockpit) || self.segments.is_empty() {
            return;
        }
        let rec = ledger::record_from(
            self.session_id.clone(),
            self.title.clone(),
            self.branch.clone(),
            &self.segments,
            &self.interactions,
        );
        let _ = ledger::append(&rec);
    }

    /// Record a submitted input into the command history (skipping consecutive duplicates)
    /// and reset history browsing.
    fn record_history(&mut self, input: &str) {
        if self.history.last().map(String::as_str) != Some(input) {
            self.history.push(input.to_string());
        }
        self.history_pos = None;
        self.draft.clear();
    }

    /// Step to an older command (Up). The live draft is stashed on the first step so it can be
    /// restored by stepping back down past the newest entry.
    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let i = match self.history_pos {
            None => {
                self.draft = std::mem::take(&mut self.chat_input);
                self.history.len() - 1
            }
            Some(0) => return, // already at the oldest
            Some(i) => i - 1,
        };
        self.history_pos = Some(i);
        self.chat_input = self.history[i].clone();
    }

    /// Step to a newer command (Down); past the newest, restore the stashed draft.
    fn history_next(&mut self) {
        let Some(i) = self.history_pos else {
            return; // not browsing
        };
        if i + 1 < self.history.len() {
            self.history_pos = Some(i + 1);
            self.chat_input = self.history[i + 1].clone();
        } else {
            self.history_pos = None;
            self.chat_input = std::mem::take(&mut self.draft);
        }
    }

    /// Cycle the active (Tab-focusable) panel forward (`dir > 0`) or backward.
    fn cycle_panel(&mut self, dir: i32) {
        let order: &[Panel] = if self.show_thinking {
            &[Panel::Activity, Panel::Segments, Panel::Thinking]
        } else {
            &[Panel::Activity, Panel::Segments]
        };
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
        // Focusing Activity auto-selects: the prior pin survives tabbing away, so it just
        // re-appears; on the first focus (nothing pinned) land on the most recent line.
        if self.active == Some(Panel::Activity) && self.activity_sel.is_none() {
            self.nav_event(0); // delta 0 → defaults to the latest event and marks it seen
        }
    }

    /// Which focusable panel (if any) sits under a terminal coordinate, using the layout from
    /// the last drawn frame. Only the Tab-focusable panels (Activity / Segments / Thinking) are
    /// reported; passive readouts and the chat spine return `None`.
    fn panel_at(&self, x: u16, y: u16) -> Option<Panel> {
        if !matches!(self.mode, Mode::Cockpit) {
            return None;
        }
        let l = cockpit_layout(self.last_area, self.show_thinking);
        let p = Position { x, y };
        if l.activity.contains(p) {
            Some(Panel::Activity)
        } else if l.segments.contains(p) {
            Some(Panel::Segments)
        } else if l.thinking.contains(p) {
            Some(Panel::Thinking)
        } else {
            None
        }
    }

    /// Focus a panel (e.g. from a mouse click), mirroring `cycle_panel`'s auto-select on Activity.
    fn focus_panel(&mut self, p: Panel) {
        self.active = Some(p);
        if p == Panel::Activity && self.activity_sel.is_none() {
            self.nav_event(0);
        }
    }

    /// Route `↑↓`/`PgUp`/`PgDn` to whichever panel is focused.
    fn nav_active(&mut self, delta: i32) {
        match self.active {
            Some(Panel::Activity) => self.nav_event(delta),
            Some(Panel::Segments) => self.nav_segment(delta),
            _ => {}
        }
    }

    /// Consume the pending vim count prefix as a positive step (default 1).
    fn take_count(&mut self) -> i32 {
        let n = self.nav_count.parse::<i32>().unwrap_or(1).max(1);
        self.nav_count.clear();
        n
    }

    /// Vim-style keys while a panel is focused: digits build a count, `j`/`k` move by it,
    /// and `g`/`G` jump to the top/bottom. Returns false for any other key so it can fall
    /// through to the chat input.
    fn try_vim_key(&mut self, c: char) -> bool {
        match c {
            '1'..='9' => self.nav_count.push(c),
            '0' if !self.nav_count.is_empty() => self.nav_count.push(c),
            'j' => {
                let n = self.take_count();
                self.nav_active(n);
            }
            'k' => {
                let n = self.take_count();
                self.nav_active(-n);
            }
            'g' => {
                self.nav_count.clear();
                self.nav_to_edge(true);
            }
            'G' => {
                self.nav_count.clear();
                self.nav_to_edge(false);
            }
            _ => return false,
        }
        true
    }

    /// Jump the focused panel to its first (`top`) or last entry.
    fn nav_to_edge(&mut self, top: bool) {
        match self.active {
            Some(Panel::Activity) => {
                let total = self.store.events.len();
                if total == 0 {
                    return;
                }
                let idx = if top { 0 } else { total - 1 };
                self.activity_sel = Some(idx);
                self.interactions.mark_seen(idx);
            }
            Some(Panel::Segments) => {
                if self.segments.is_empty() {
                    return;
                }
                let idx = if top { 0 } else { self.segments.len() - 1 };
                self.segments_sel = Some(idx);
                let start = self.segments[idx].start_idx;
                self.activity_sel = Some(start);
                self.interactions.mark_seen(start);
            }
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

    /// Segment incrementally from the start of the last block (extends it / adds new blocks),
    /// or do a full re-segmentation when `force`.
    /// Kick off batched segmentation. `--force` discards existing segments and re-segments from
    /// the start; otherwise it resumes from the watermark. Batches of [`BATCH_SIZE`] events
    /// auto-chain in [`Self::on_seg_msg`] until the whole session is covered.
    fn start_segmentation(&mut self, force: bool, tx: &UnboundedSender<SegMsg>) {
        if self.segments_loading {
            return;
        }
        if self.provider.is_none() {
            self.chat_log.push(ChatEntry::system("No model configured — can't segment.".into()));
            return;
        }
        if force {
            self.segments.clear();
            self.segments_sel = None;
            self.seg_watermark = 0;
        }
        if self.seg_watermark >= self.store.events.len() {
            self.chat_log.push(ChatEntry::system("No new activity to segment yet.".into()));
            return;
        }
        // Fresh run: reset the per-batch timing used for the progress average.
        self.seg_elapsed = Duration::ZERO;
        self.seg_batches_done = 0;
        self.fire_batch(tx);
    }

    /// Run one batch — the next [`BATCH_SIZE`] events from the watermark — as a detached stream.
    /// Existing segments are frozen; the model only decides where new work begins within the
    /// batch (and whether the batch continues the previous segment). Leading events that produce
    /// no listing are skipped forward without a model call. Assembled in [`Self::on_seg_msg`].
    fn fire_batch(&mut self, tx: &UnboundedSender<SegMsg>) {
        if self.segments_loading || self.provider.is_none() {
            return;
        }
        let n = self.store.events.len();
        let mut start = self.seg_watermark;
        let (messages, map) = loop {
            if start >= n {
                self.seg_watermark = n;
                self.persist_cache();
                return;
            }
            let prev_title = self.segments.last().map(|s| s.title.clone());
            let (messages, map) = segment_batch_request(&self.store, start, prev_title.as_deref());
            if !map.is_empty() {
                break (messages, map);
            }
            start = (start + BATCH_SIZE).min(n); // batch had no renderable events; skip ahead
        };
        // Freeze every existing segment; this batch only appends new boundaries beyond them.
        self.seg_frozen = self.segments.iter().map(|s| (s.start_idx, s.title.clone())).collect();
        self.seg_batch_start = start;
        self.seg_map = map;
        self.seg_progress.clear();
        self.seg_batch_started_at = Some(Instant::now());
        self.segments_loading = true;
        let stream = self.provider.as_ref().unwrap().stream_chat(messages);
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut stream = stream;
            let mut out = String::new();
            let mut err = None;
            let mut found = 0;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(d) => {
                        out.push_str(&d);
                        // Surface each newly-completed boundary as it streams in.
                        let titles = parse_partial_boundaries(&out);
                        if titles.len() != found {
                            found = titles.len();
                            let _ = tx.send(SegMsg::Progress(
                                titles.into_iter().map(|(_, t)| t).collect(),
                            ));
                        }
                    }
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

    fn on_seg_msg(&mut self, msg: SegMsg, tx: &UnboundedSender<SegMsg>) {
        match msg {
            SegMsg::Progress(titles) => self.seg_progress = titles,
            SegMsg::Done(raw) => {
                self.segments_loading = false;
                self.seg_progress.clear();
                if let Some(t0) = self.seg_batch_started_at.take() {
                    self.seg_elapsed += t0.elapsed();
                    self.seg_batches_done += 1;
                }
                // Frozen blocks + this batch's boundaries → full segment list. With a previous
                // segment, the batch may add no boundary at its head, folding leading events into
                // that segment; the first batch always anchors a start at 0.
                let has_prev = !self.seg_frozen.is_empty();
                let mut starts = batch_starts(&raw, &self.seg_map, has_prev);
                starts.append(&mut self.seg_frozen);
                self.segments = build_segments_from_starts(starts, &self.store);
                self.segments_sel = None;
                let n = self.store.events.len();
                self.seg_watermark = (self.seg_batch_start + BATCH_SIZE).min(n);
                self.persist_cache();
                if self.seg_watermark < n {
                    self.fire_batch(tx); // auto-chain to the next batch
                }
            }
            SegMsg::Error(e) => {
                self.segments_loading = false;
                self.seg_progress.clear();
                self.seg_frozen.clear();
                self.chat_log.push(ChatEntry::system(format!("Segmentation failed: {e}")));
            }
        }
    }

    /// Apply the persisted cache once the backfill is in: rebuild cached segments against the
    /// real store, then segment only the new tail (or do a full pass if there's no usable
    /// cache). Called the first time events arrive after attaching.
    fn reconcile_on_backfill(&mut self, tx: &UnboundedSender<SegMsg>) {
        let n = self.store.events.len();
        match self.pending_cache.take() {
            // Usable cache: rebuild its segments locally (no model call for the old part).
            Some(c) if c.watermark <= n && !c.segments.is_empty() => {
                self.segments = build_segments_from_starts(c.starts(), &self.store);
                self.seg_watermark = c.watermark;
                self.segments_sel = None;
                if n > c.watermark {
                    self.chat_log
                        .push(ChatEntry::system("new activity since last visit — segmenting the tail…".into()));
                    self.start_segmentation(false, tx); // incremental from the last block
                }
            }
            // No (or stale) cache: full segmentation of the history.
            _ => {
                self.chat_log.push(ChatEntry::system("auto-segmenting session history…".into()));
                self.start_segmentation(true, tx);
            }
        }
        // Recompute the summary only if the restored one doesn't already cover these events.
        self.summary_dirty = self.summary_len < n;
    }

    /// Write the current segments + summary to the per-session cache (best-effort).
    fn persist_cache(&self) {
        if self.session_id.is_empty() {
            return;
        }
        let cache = session_cache::SessionCache::from_state(
            self.session_id.clone(),
            self.seg_watermark,
            &self.segments,
            self.glance_summary.clone(),
            self.summary_len,
        );
        let _ = session_cache::save(&cache);
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
        self.record_history(&input);
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
            "/segments" => {
                // Default: incremental (extend the last block, add new ones). --force re-segments.
                let force = matches!(arg, "--force" | "-f");
                self.start_segmentation(force, seg_tx);
            }
            "/debt" => self.cmd_debt(),
            "/explain" => self.start_explain(arg, explain_tx),
            "/show" => self.cmd_show(arg),
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

    /// Toggle visibility of a named panel: `/show thinking`. When the Thinking panel is
    /// hidden, Detail takes its whole right column.
    fn cmd_show(&mut self, arg: &str) {
        match arg {
            "thinking" => {
                self.show_thinking = !self.show_thinking;
                // Don't leave focus stranded on a now-hidden panel.
                if !self.show_thinking && self.active == Some(Panel::Thinking) {
                    self.active = None;
                }
                let state = if self.show_thinking { "shown" } else { "hidden" };
                self.chat_log
                    .push(ChatEntry::system(format!("thinking panel {state}")));
            }
            "" => self
                .chat_log
                .push(ChatEntry::system("usage: /show thinking".into())),
            other => self
                .chat_log
                .push(ChatEntry::system(format!("unknown panel: {other} (try /show thinking)"))),
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
                self.summary_len = self.store.events.len();
                self.persist_cache();
            }
        }
        // On error, keep the previous summary rather than blanking the panel.
    }
}

pub async fn run() -> Result<()> {
    let mut terminal = ratatui::init();
    // Capture the mouse so we can scroll the hovered panel, click to focus, and drag-select.
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    let result = event_loop(&mut terminal).await;
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
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
                match maybe {
                    Some(Ok(CEvent::Key(key))) if key.kind == KeyEventKind::Press => {
                        handle_key(&mut app, key.code, key.modifiers, &channels);
                    }
                    Some(Ok(CEvent::Mouse(m))) => handle_mouse(&mut app, m),
                    _ => {}
                }
            }
            Some(events) = ev_rx.recv() => {
                if !events.is_empty() {
                    for e in events { app.store.add(e); }
                    app.summary_dirty = true;
                    app.last_event_at = Instant::now();
                }
                // First events for a session: apply any cache, then segment only the new tail.
                if app.auto_segment_pending && !app.store.events.is_empty() {
                    app.auto_segment_pending = false;
                    app.reconcile_on_backfill(&channels.seg);
                }
            }
            Some(msg) = chat_rx.recv() => app.on_chat_msg(msg),
            Some(msg) = sum_rx.recv() => app.on_summary_msg(msg),
            Some(msg) = seg_rx.recv() => app.on_seg_msg(msg, &channels.seg),
            Some(msg) = tag_rx.recv() => app.on_tag_msg(msg),
            Some(msg) = explain_rx.recv() => app.on_explain_msg(msg),
            _ = tick.tick() => {}
        }
        app.maybe_summarize(&sum_tx);
    }
}

fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers, ch: &Channels) {
    // Any keyboard interaction dismisses a lingering mouse selection highlight.
    app.sel_anchor = None;
    app.sel_cursor = None;

    // Global quit (works even while typing in the chat input).
    if matches!(code, KeyCode::Char('c') | KeyCode::Char('q')) && mods.contains(KeyModifiers::CONTROL) {
        app.persist();
        app.should_quit = true;
        return;
    }

    match app.mode {
        // Type to fuzzy-filter; arrows select; Tab cycles the agent filter. (^q quits, above.)
        Mode::Picker => match code {
            KeyCode::Up => move_picker(app, -1),
            KeyCode::Down => move_picker(app, 1),
            KeyCode::Enter => app.attach(&ch.ev),
            KeyCode::Tab => app.cycle_agent_filter(1),
            KeyCode::BackTab => app.cycle_agent_filter(-1),
            // Esc clears the typed query first, then the agent filter.
            KeyCode::Esc => {
                if !app.picker_query.is_empty() {
                    app.picker_query.clear();
                    app.reset_picker_selection();
                } else if app.agent_filter.is_some() {
                    app.agent_filter = None;
                    app.reset_picker_selection();
                }
            }
            KeyCode::Backspace => {
                if app.picker_query.pop().is_some() {
                    app.reset_picker_selection();
                }
            }
            KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
                app.picker_query.push(c);
                app.reset_picker_selection();
            }
            _ => {}
        },
        // Chat-first: typing always goes to the input; Tab cycles which panel scrolls.
        Mode::Cockpit => match code {
            KeyCode::Tab => {
                app.nav_count.clear();
                app.cycle_panel(1);
            }
            KeyCode::BackTab => {
                app.nav_count.clear();
                app.cycle_panel(-1);
            }
            // Three-stage Esc: unfocus panel → unpin selection (resume live) → back to picker.
            KeyCode::Esc => {
                app.nav_count.clear();
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
            // With a panel focused, ↑↓ scroll it (honoring a vim count); in pure chat mode
            // they walk command history.
            KeyCode::Up if app.active.is_some() => {
                let n = app.take_count();
                app.nav_active(-n);
            }
            KeyCode::Down if app.active.is_some() => {
                let n = app.take_count();
                app.nav_active(n);
            }
            KeyCode::Up => app.history_prev(),
            KeyCode::Down => app.history_next(),
            KeyCode::PageUp => app.nav_active(-10),
            KeyCode::PageDown => app.nav_active(10),
            // While a panel is focused, vim motions (j/k, g/G, count prefix) navigate it;
            // every other character still goes to the chat input.
            KeyCode::Char(c) if app.active.is_some() && app.try_vim_key(c) => {}
            KeyCode::Char(c) => {
                app.nav_count.clear();
                app.chat_input.push(c);
            }
            _ => {}
        },
    }
}

/// Mouse routing: wheel scrolls the hovered panel, left-drag selects text (copied on release),
/// and a left-click (press+release without moving) focuses the panel under the cursor.
fn handle_mouse(app: &mut App, m: MouseEvent) {
    let (x, y) = (m.column, m.row);
    match m.kind {
        MouseEventKind::ScrollUp => scroll_at(app, x, y, -1),
        MouseEventKind::ScrollDown => scroll_at(app, x, y, 1),
        MouseEventKind::Down(MouseButton::Left) => {
            // Begin a potential drag-selection; a release without movement becomes a click.
            app.sel_anchor = Some((x, y));
            app.sel_cursor = Some((x, y));
            app.copy_pending = false;
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if app.sel_anchor.is_some() {
                app.sel_cursor = Some((x, y));
            }
        }
        MouseEventKind::Up(MouseButton::Left) => finish_left_button(app, x, y),
        _ => {}
    }
}

/// Scroll whatever the cursor is over — no focus change required.
fn scroll_at(app: &mut App, x: u16, y: u16, delta: i32) {
    match app.mode {
        Mode::Picker => move_picker(app, delta),
        Mode::Cockpit => match app.panel_at(x, y) {
            Some(Panel::Activity) => app.nav_event(delta),
            Some(Panel::Segments) => app.nav_segment(delta),
            _ => {} // Thinking / Detail / Glance / chat carry no scroll model
        },
    }
}

/// Resolve a left-button release: a real drag requests a clipboard copy; a stationary click
/// focuses the panel underneath (and dismisses any empty selection).
fn finish_left_button(app: &mut App, x: u16, y: u16) {
    match app.sel_anchor {
        Some(a) if a != (x, y) => {
            app.sel_cursor = Some((x, y));
            app.copy_pending = true; // text is extracted from the buffer on the next draw
        }
        Some(_) => {
            app.sel_anchor = None;
            app.sel_cursor = None;
            if let Some(p) = app.panel_at(x, y) {
                app.focus_panel(p);
            }
        }
        None => {}
    }
}

/// Order two points in reading order (row, then column).
fn order_points(a: (u16, u16), b: (u16, u16)) -> ((u16, u16), (u16, u16)) {
    if (a.1, a.0) <= (b.1, b.0) {
        (a, b)
    } else {
        (b, a)
    }
}

/// Column span `[x0, x1]` selected on row `y` for a linear selection from `start` to `end`.
fn row_span(y: u16, start: (u16, u16), end: (u16, u16), width: u16) -> (u16, u16) {
    let last = width.saturating_sub(1);
    if start.1 == end.1 {
        (start.0, end.0)
    } else if y == start.1 {
        (start.0, last)
    } else if y == end.1 {
        (0, end.0)
    } else {
        (0, last)
    }
}

/// Highlight the active drag-selection (reversed video) and, when a copy was just requested,
/// extract the selected cells from the freshly rendered buffer onto the system clipboard.
fn apply_selection(f: &mut Frame, app: &mut App) {
    let (Some(anchor), Some(cursor)) = (app.sel_anchor, app.sel_cursor) else {
        return;
    };
    if anchor == cursor {
        return; // a click, or a drag that hasn't moved yet — nothing to highlight
    }
    let width = f.area().width;
    let (start, end) = order_points(anchor, cursor);
    let buf = f.buffer_mut();

    // Reversed video over the selected cells.
    for y in start.1..=end.1 {
        let (x0, x1) = row_span(y, start, end, width);
        for x in x0..=x1 {
            if let Some(cell) = buf.cell_mut(Position { x, y }) {
                cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
            }
        }
    }

    if app.copy_pending {
        app.copy_pending = false;
        copy_to_clipboard(&selection_text(buf, start, end, width));
    }
}

/// Extract the selected region of `buf` as text: trailing spaces trimmed per row, rows joined
/// by newlines. `start`/`end` are in reading order (see `order_points`).
fn selection_text(buf: &Buffer, start: (u16, u16), end: (u16, u16), width: u16) -> String {
    let mut text = String::new();
    for y in start.1..=end.1 {
        let (x0, x1) = row_span(y, start, end, width);
        let mut line = String::new();
        for x in x0..=x1 {
            if let Some(cell) = buf.cell(Position { x, y }) {
                line.push_str(cell.symbol());
            }
        }
        text.push_str(line.trim_end());
        if y < end.1 {
            text.push('\n');
        }
    }
    text
}

/// Best-effort write to the system clipboard.
fn copy_to_clipboard(text: &str) {
    if text.is_empty() {
        return;
    }
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(text.to_string());
    }
}

/// Case-insensitive subsequence test: do all chars of `needle` appear in `haystack`, in order?
/// Both arguments are expected to be already lowercased.
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut hay = haystack.chars();
    'next: for nc in needle.chars() {
        for hc in hay.by_ref() {
            if hc == nc {
                continue 'next;
            }
        }
        return false; // ran out of haystack before matching this needle char
    }
    true
}

fn move_picker(app: &mut App, delta: i32) {
    let n = app.visible_indices().len() as i32;
    if n == 0 {
        return;
    }
    let cur = app.picker.selected().unwrap_or(0) as i32;
    app.picker.select(Some((cur + delta).rem_euclid(n) as usize));
}

fn ui(f: &mut Frame, app: &mut App) {
    app.last_area = f.area(); // remembered for mouse hit-testing between frames
    match app.mode {
        Mode::Picker => draw_picker(f, app),
        Mode::Cockpit => draw_cockpit(f, app),
    }
    apply_selection(f, app); // drag-highlight on top of everything; copies when released
}

fn draw_picker(f: &mut Frame, app: &mut App) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(f.area());
    let nerd = use_nerd_icons();
    let visible = app.visible_indices();
    let items: Vec<ListItem> = visible
        .iter()
        .map(|&i| {
            let s = &app.sessions[i];
            let project = short_project(&s.cwd);
            let branch = if s.git_branch.is_empty() { "—" } else { &s.git_branch };
            ListItem::new(Line::from(vec![
                agent_cell(s.agent, nerd),
                Span::raw("  "),
                Span::styled(format!("{project:<22.22} "), Style::default().fg(Color::Cyan)),
                Span::styled(format!("{branch:<12.12} "), Style::default().fg(Color::DarkGray)),
                Span::raw(truncate(&s.summary, 50)),
            ]))
        })
        .collect();
    // Title carries the active agent filter; "all" when unset.
    let scope = app.agent_filter.map(Agent::name).unwrap_or("all");
    let title = format!(
        " Understudy — select a session (read-only)  ·  agent: {scope} [{}/{}] ",
        visible.len(),
        app.sessions.len()
    );
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().bg(Color::Blue).add_modifier(Modifier::BOLD))
        .highlight_symbol("▌");
    f.render_stateful_widget(list, chunks[0], &mut app.picker);
    f.render_widget(draw_picker_footer(app), chunks[1]);
}

/// Picker footer: the live fuzzy query (with cursor) when typing, else the key hints.
fn draw_picker_footer(app: &App) -> Paragraph<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    if app.picker_query.is_empty() {
        Paragraph::new(" type to filter · ↑↓ select · tab agent · enter attach · ^q quit").style(dim)
    } else {
        Paragraph::new(Line::from(vec![
            Span::styled(" filter: ", dim),
            Span::styled(app.picker_query.clone(), Style::default().fg(Color::Yellow)),
            Span::styled("▌", Style::default().fg(Color::Yellow)),
            Span::styled("  ·  esc clears · tab agent · enter attach", dim),
        ]))
    }
}

/// Nerd Font glyphs can't be reliably detected from inside a terminal, so they're opt-in via
/// `UNDERSTUDY_ICONS=nerd`; otherwise the picker uses colored text tags.
fn use_nerd_icons() -> bool {
    std::env::var("UNDERSTUDY_ICONS").map(|v| v.eq_ignore_ascii_case("nerd")).unwrap_or(false)
}

/// A colored "<icon> <name>" cell identifying which coding agent a session came from.
fn agent_cell(agent: Agent, nerd: bool) -> Span<'static> {
    let icon = if nerd { agent_glyph(agent) } else { agent.tag() };
    Span::styled(format!("{icon} {:<11.11}", agent.name()), Style::default().fg(agent_color(agent)))
}

fn agent_color(agent: Agent) -> Color {
    match agent {
        Agent::ClaudeCode => Color::Rgb(217, 119, 87), // Anthropic clay
        Agent::OpenCode => Color::Cyan,
        Agent::Copilot => Color::Green,
        Agent::Codex => Color::Magenta,
        Agent::Antigravity => Color::Rgb(138, 116, 249), // Antigravity violet
        Agent::Unknown => Color::DarkGray,
    }
}

/// Nerd Font (v3, Font Awesome range) glyphs per agent; only rendered in nerd-icon mode.
fn agent_glyph(agent: Agent) -> &'static str {
    match agent {
        Agent::ClaudeCode => "\u{f544}", // robot
        Agent::OpenCode => "\u{f121}",   // code
        Agent::Copilot => "\u{f09b}",    // github
        Agent::Codex => "\u{f120}",      // terminal
        Agent::Antigravity => "\u{f135}", // rocket
        Agent::Unknown => "\u{f059}",    // question-circle
    }
}

/// Where each cockpit panel landed this frame. Absent panels (narrow layout, hidden Thinking)
/// are `Rect::default()` (zero-area), so hit-tests against them never match. Computed once and
/// shared by the renderer and mouse hit-testing so the two can't drift.
#[derive(Clone, Copy, Default)]
struct CockpitLayout {
    status: Rect,
    glance: Rect,
    segments: Rect,
    activity: Rect,
    thinking: Rect,
    detail: Rect,
    chat: Rect,
    footer: Rect,
}

fn cockpit_layout(area: Rect, show_thinking: bool) -> CockpitLayout {
    let v = Layout::vertical([
        Constraint::Length(1),       // status bar
        Constraint::Min(0),          // panel grid
        Constraint::Length(CHAT_H),  // chat spine
        Constraint::Length(1),       // footer
    ])
    .split(area);

    let mut l = CockpitLayout { status: v[0], chat: v[2], footer: v[3], ..Default::default() };
    let body = v[1];
    if body.width >= WIDE_COLS && body.height >= WIDE_ROWS {
        let cols = Layout::horizontal([
            Constraint::Percentage(28),
            Constraint::Min(0),
            Constraint::Percentage(32),
        ])
        .split(body);
        let left = Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).split(cols[0]);
        l.glance = left[0];
        l.segments = left[1];
        l.activity = cols[1];
        if show_thinking {
            let right = Layout::vertical([Constraint::Percentage(45), Constraint::Percentage(55)]).split(cols[2]);
            l.thinking = right[0];
            l.detail = right[1];
        } else {
            l.detail = cols[2];
        }
    } else {
        // Stacked fallback: Glance + Activity, chat never sacrificed.
        let rows = Layout::vertical([Constraint::Length(6), Constraint::Min(0)]).split(body);
        l.glance = rows[0];
        l.activity = rows[1];
    }
    l
}

fn draw_cockpit(f: &mut Frame, app: &App) {
    let l = cockpit_layout(f.area(), app.show_thinking);

    // Comprehension Coverage is only meaningful once segments exist.
    let report = (!app.segments.is_empty()).then(|| coverage(&app.segments, &app.interactions));

    draw_status(f, app, l.status, report.as_ref());
    draw_glance(f, app, l.glance);
    draw_activity(f, app, l.activity);
    if !l.segments.is_empty() {
        draw_segments(f, app, l.segments, report.as_ref());
    }
    if !l.detail.is_empty() {
        draw_detail(f, app, l.detail);
    }
    if !l.thinking.is_empty() {
        draw_thinking(f, app, l.thinking);
    }
    draw_chat_spine(f, app, l.chat);
    draw_footer(f, app, l.footer);
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

    if app.segments.is_empty() && !app.segments_loading {
        f.render_widget(
            Paragraph::new("(none yet — /segments)").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }
    let width = inner.width.saturating_sub(5) as usize;
    // Committed segments accumulate as batches land; a footer shows the in-flight batch.
    let mut items: Vec<ListItem> = app
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
    if app.segments_loading {
        let n = app.store.events.len();
        let batch_end = (app.seg_batch_start + BATCH_SIZE).min(n);
        let mut head = format!("⟳ segmenting {batch_end}/{n} events");
        if app.seg_batches_done > 0 {
            let avg = app.seg_elapsed.as_secs_f32() / app.seg_batches_done as f32;
            head.push_str(&format!(" · {avg:.1}s/batch"));
            let remaining = n.saturating_sub(batch_end).div_ceil(BATCH_SIZE);
            if remaining > 0 {
                head.push_str(&format!(" · ~{}s left", (remaining as f32 * avg).ceil() as u64));
            }
        }
        items.push(ListItem::new(Line::styled(head, Style::default().fg(Color::Yellow))));
        if app.seg_progress.is_empty() {
            items.push(ListItem::new(Line::styled(
                "  reading the activity log…",
                Style::default().fg(Color::DarkGray),
            )));
        }
        for t in &app.seg_progress {
            items.push(ListItem::new(Line::from(vec![
                Span::raw("  "),
                Span::raw(truncate(t, width)),
            ])));
        }
    }
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
    // While a panel is focused, surface the vim motions; otherwise the chat-mode hint.
    let nav = if app.active.is_some() { "j k (10j) · g G ends" } else { "↑↓ select" };
    let hint = format!(" tab panes · {nav} · enter send · /segments · {esc} · ^q quit");
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
    use understudy_core::sources::Source;

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
        app.show_thinking = true; // Thinking is opt-in (/show thinking); enable it to assert the full layout.
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

    fn session(agent: Agent, summary: &str) -> SessionInfo {
        SessionInfo {
            agent,
            path: PathBuf::new(),
            session_id: summary.to_string(),
            cwd: "/work/proj".into(),
            git_branch: "main".into(),
            modified: std::time::SystemTime::UNIX_EPOCH,
            size: 0,
            summary: summary.into(),
        }
    }

    fn picker_app(sessions: Vec<SessionInfo>) -> App {
        let mut app = App::new();
        app.sessions = sessions;
        app.picker_query.clear();
        app.agent_filter = None;
        app.reset_picker_selection();
        app
    }

    #[test]
    fn fuzzy_query_filters_on_agent_and_title() {
        let mut app = picker_app(vec![
            session(Agent::ClaudeCode, "Fix the parser"),
            session(Agent::OpenCode, "Parse config flags"),
            session(Agent::Copilot, "Refactor renderer"),
        ]);
        // Tokens span agent name + title, as a subsequence: "claud pars" → "claude code … parser".
        app.picker_query = "claud pars".into();
        assert_eq!(app.visible_indices(), vec![0]);
        // A token can match the title alone.
        app.picker_query = "refac".into();
        assert_eq!(app.visible_indices(), vec![2]);
        // …or the agent name.
        app.picker_query = "opencode".into();
        assert_eq!(app.visible_indices(), vec![1]);
        // No match.
        app.picker_query = "zzqq".into();
        assert!(app.visible_indices().is_empty());
    }

    #[test]
    fn tab_cycles_agent_filter_through_present_agents_only() {
        // OpenCode is absent, so the cycle must skip it.
        let mut app = picker_app(vec![
            session(Agent::ClaudeCode, "a"),
            session(Agent::Copilot, "b"),
        ]);
        assert_eq!(app.agent_filter, None); // all
        app.cycle_agent_filter(1);
        assert_eq!(app.agent_filter, Some(Agent::ClaudeCode));
        app.cycle_agent_filter(1);
        assert_eq!(app.agent_filter, Some(Agent::Copilot)); // skipped OpenCode
        app.cycle_agent_filter(1);
        assert_eq!(app.agent_filter, None); // wrapped back to all
        app.cycle_agent_filter(-1);
        assert_eq!(app.agent_filter, Some(Agent::Copilot)); // reverse
    }

    #[test]
    fn selection_follows_filtered_list() {
        let mut app = picker_app(vec![
            session(Agent::ClaudeCode, "alpha"),
            session(Agent::OpenCode, "beta"),
            session(Agent::Copilot, "gamma"),
        ]);
        app.agent_filter = Some(Agent::Copilot);
        app.reset_picker_selection();
        // Only "gamma" is visible; row 0 must resolve back to sessions[2].
        assert_eq!(app.selected_session().map(|s| s.summary.as_str()), Some("gamma"));
    }

    #[test]
    fn empty_result_clears_selection() {
        let mut app = picker_app(vec![session(Agent::ClaudeCode, "alpha")]);
        app.picker_query = "zzz".into();
        app.reset_picker_selection();
        assert_eq!(app.picker.selected(), None);
        assert!(app.selected_session().is_none());
    }

    fn wide_cockpit() -> (App, CockpitLayout) {
        let mut app = cockpit_app();
        app.show_thinking = true;
        app.last_area = Rect::new(0, 0, 130, 40);
        let l = cockpit_layout(app.last_area, true);
        (app, l)
    }

    fn center(r: Rect) -> (u16, u16) {
        (r.x + r.width / 2, r.y + r.height / 2)
    }

    #[test]
    fn panel_at_hit_tests_only_focusable_panels() {
        let (app, l) = wide_cockpit();
        let (ax, ay) = center(l.activity);
        assert_eq!(app.panel_at(ax, ay), Some(Panel::Activity));
        let (sx, sy) = center(l.segments);
        assert_eq!(app.panel_at(sx, sy), Some(Panel::Segments));
        let (tx, ty) = center(l.thinking);
        assert_eq!(app.panel_at(tx, ty), Some(Panel::Thinking));
        // Passive readouts and the chat spine never report a focusable panel.
        assert_eq!(app.panel_at(center(l.glance).0, center(l.glance).1), None);
        assert_eq!(app.panel_at(center(l.detail).0, center(l.detail).1), None);
        assert_eq!(app.panel_at(center(l.chat).0, center(l.chat).1), None);
    }

    #[test]
    fn click_focuses_panel_drag_requests_copy() {
        let (mut app, l) = wide_cockpit();
        // Stationary press+release on Segments = a click → focus it.
        let (sx, sy) = center(l.segments);
        app.sel_anchor = Some((sx, sy));
        app.sel_cursor = Some((sx, sy));
        finish_left_button(&mut app, sx, sy);
        assert_eq!(app.active, Some(Panel::Segments));
        assert!(app.sel_anchor.is_none()); // click clears the empty selection

        // A drag (moved before release) requests a copy and does not focus.
        app.active = None;
        app.sel_anchor = Some((sx, sy));
        app.sel_cursor = Some((sx, sy));
        finish_left_button(&mut app, sx + 4, sy + 1);
        assert!(app.copy_pending);
        assert_eq!(app.sel_cursor, Some((sx + 4, sy + 1)));
        assert_eq!(app.active, None);
    }

    #[test]
    fn scroll_moves_hovered_panel() {
        let (mut app, l) = wide_cockpit();
        let total = app.store.events.len();
        assert!(total >= 2, "fixture needs at least two events");
        // Hover Activity and scroll up: selection steps back one event from the latest.
        app.activity_sel = None;
        let (ax, ay) = center(l.activity);
        scroll_at(&mut app, ax, ay, -1);
        assert_eq!(app.activity_sel, Some(total - 2));
        // Hovering a non-scrollable panel (Detail) does nothing.
        let pinned = app.activity_sel;
        scroll_at(&mut app, center(l.detail).0, center(l.detail).1, -1);
        assert_eq!(app.activity_sel, pinned);
    }

    #[test]
    fn order_points_sorts_reading_order() {
        assert_eq!(order_points((7, 2), (3, 0)), ((3, 0), (7, 2)));
        assert_eq!(order_points((5, 1), (2, 1)), ((2, 1), (5, 1))); // same row → by column
    }

    #[test]
    fn row_span_widens_interior_rows() {
        assert_eq!(row_span(0, (3, 0), (7, 0), 80), (3, 7)); // single row
        assert_eq!(row_span(0, (3, 0), (7, 2), 80), (3, 79)); // first row → to right edge
        assert_eq!(row_span(1, (3, 0), (7, 2), 80), (0, 79)); // middle row → full width
        assert_eq!(row_span(2, (3, 0), (7, 2), 80), (0, 7)); // last row → from left edge
    }

    #[test]
    fn selection_text_extracts_and_trims() {
        let buf = Buffer::with_lines(vec!["hello world   ", "second line   "]);
        let w = buf.area.width;
        // Single row, "hello".
        assert_eq!(selection_text(&buf, (0, 0), (4, 0), w), "hello");
        // Across rows: row0 from x6 to its (trimmed) end, then row0..=5 of row1.
        assert_eq!(selection_text(&buf, (6, 0), (5, 1), w), "world\nsecond");
    }

    #[test]
    fn tab_cycles_active_panel_then_wraps_to_none() {
        let mut app = cockpit_app();
        app.show_thinking = true; // include the opt-in Thinking panel in the focus cycle
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
    fn persist_writes_a_ledger_record() {
        let tmp = std::env::temp_dir().join(format!("understudy_tui_ledger_{}.jsonl", std::process::id()));
        std::env::set_var("UNDERSTUDY_LEDGER", &tmp);
        let _ = std::fs::remove_file(&tmp);

        let mut app = cockpit_app();
        app.session_id = "sX".into();
        app.segments = vec![fake_segment("a", 0, 5)];
        app.persist();

        let recs = understudy_core::comprehension::ledger::read_all();
        assert!(recs.iter().any(|r| r.session_id == "sX" && r.project == "proj"));

        let _ = std::fs::remove_file(&tmp);
        std::env::remove_var("UNDERSTUDY_LEDGER");
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
        let (seg_tx, _r) = mpsc::unbounded_channel::<SegMsg>();
        app.on_seg_msg(SegMsg::Done("[{\"start\":0,\"title\":\"All\"}]".into()), &seg_tx);
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
