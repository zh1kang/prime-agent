//! Native workspace pages inside pa: conversation history (`/history`, `/threads`,
//! `/resume history`), the machine editor (`/fsm edit`, factory `e`), and git review
//! (`/review`, Captain `v`).
//!
//! The view is presentation and selection only. pa-cli owns the data and every change:
//! the session UI runs `pa thread ARGS` with one JSON request for each
//! [`WorkspaceAction::Call`] and hands the JSON answer back through [`WorkspaceView::apply`].
//! Text input goes through pa's prompt editor ([`WorkspaceAction::Compose`]). The only screen
//! outside pa is the optional "edit file in $EDITOR" key on a library machine draft.

use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{json, Value};

use crate::keybindings::KeybindingsManager;
use crate::menu_panel::{hint_row, menu_row, no_match_row, rule_row, MenuSegment};
use crate::search_input::SearchInput;
use crate::theme::{Theme, ThemeColor};
use crate::width::{line_width, str_width, truncate_line, truncate_to_width};
use crate::{Line, Span};

#[cfg(test)]
mod tests;

/// Which call a background answer belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceRequest {
    HistoryList,
    HistoryOpen,
    Machines,
    Draft,
    States,
    SetModel,
    Read,
    Write,
    Save,
    Discard,
    Files,
    Diff,
    Stage,
    /// A crew worktree's branch merged into its main checkout.
    Merge,
    /// A crew review's structured findings (best effort: an unsupported backend shows none).
    Findings,
    /// One finding's triage status saved.
    Triage,
    /// A crew worktree rolled back to its last pre-turn checkpoint.
    Rollback,
    /// The daemon's `absorb_context` answer, delivered after the history page closed.
    Absorb,
}

/// The view's answer to a key or a call result.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WorkspaceAction {
    None,
    Close,
    /// Run `pa thread ARGS` with `input` on stdin; the answer returns to [`WorkspaceView::apply`].
    Call {
        request: WorkspaceRequest,
        args: Vec<String>,
        input: Value,
    },
    /// Resume this native session file.
    Resume(String),
    /// Ask for text in pa's prompt editor; the answer returns to [`WorkspaceView::compose_done`].
    Compose {
        label: String,
        initial: String,
    },
    /// Hand the terminal to `$VISUAL`/`$EDITOR` on this file, then call
    /// [`WorkspaceView::file_edited`].
    EditFile(String),
    /// Open pa's shared searchable model + thinking picker for a state of the open draft.
    PickModel {
        draft: String,
        state_id: String,
        role: Option<String>,
        current_model: String,
        current_thinking: String,
    },
    /// Merge the branch checked out in this crew worktree into its main checkout; the answer
    /// returns to [`WorkspaceView::apply`] as [`WorkspaceRequest::Merge`].
    MergeWorktree(String),
    /// Absorb these earlier chats (`absorb_context` sources) into the current chat.
    Absorb {
        sources: Vec<Value>,
        focus: Option<String>,
    },
}

fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn call(request: WorkspaceRequest, args: &[&str], input: Value) -> WorkspaceAction {
    WorkspaceAction::Call {
        request,
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        input,
    }
}

// History

#[derive(Debug, Clone)]
struct HistoryRow {
    title: String,
    search: String,
    target: Value,
    raw_bytes: u64,
    cached_tokens: Option<u64>,
}

/// Bytes per token for a transcript on disk, and the packet a fresh distill aims for.
const BYTES_PER_TOKEN: f64 = 4.5;
const PACKET_ESTIMATE_TOKENS: u64 = 1500;

impl HistoryRow {
    fn is_prime(&self) -> bool {
        self.target[0].as_str() == Some("prime")
    }

    /// The packet this row adds: the cached digest's size, else an estimate.
    fn packet_tokens(&self) -> u64 {
        self.cached_tokens.unwrap_or_else(|| {
            ((self.raw_bytes as f64 / BYTES_PER_TOKEN) as u64).min(PACKET_ESTIMATE_TOKENS)
        })
    }

    /// `97k raw · ~1.4k`, or `97k raw · ~1.5k est` without a cached digest.
    fn cost_preview(&self) -> Option<String> {
        if self.raw_bytes == 0 && self.cached_tokens.is_none() {
            return None;
        }
        let packet = compact_count(self.packet_tokens());
        Some(match self.cached_tokens {
            Some(_) => format!("{} raw · ~{packet}", compact_count(self.raw_bytes)),
            None => format!("{} raw · ~{packet} est", compact_count(self.raw_bytes)),
        })
    }
}

/// `950`, `1.4k`, `97k`, `1.2M`.
pub(crate) fn compact_count(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=9_999 => format!("{:.1}k", n as f64 / 1_000.0),
        10_000..=999_999 => format!("{}k", (n + 500) / 1_000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// One `absorb_context` source for a history row's `[provider, selector, location]` target.
fn absorb_source(target: &Value) -> Value {
    let part = |i: usize| target[i].as_str().unwrap_or_default();
    let (provider, selector, location) = (part(0), part(1), part(2));
    match provider {
        "t3" => json!({"provider": provider, "id": selector}),
        "pa" if location.is_empty() => json!({"provider": provider, "id": selector}),
        "pa" => json!({
            "provider": provider,
            "id": selector,
            "path": format!("{location}/threads/{selector}.json"),
        }),
        "prime" => json!({"provider": provider, "path": selector}),
        _ => json!({"provider": provider, "id": selector, "path": location}),
    }
}

#[derive(Debug)]
struct History {
    groups: Vec<(String, Vec<HistoryRow>)>,
    tab: usize,
    selected: usize,
    search: SearchInput,
    /// Opened by `/absorb`: Enter absorbs every row into the current chat.
    absorb_mode: bool,
    /// The rows checked with Space, by target.
    selected_targets: Vec<Value>,
    focus: Option<String>,
    /// The prompt editor is asking for the focus topic.
    composing_focus: bool,
}

impl History {
    fn items(&self) -> Vec<&HistoryRow> {
        let term = self.search.value().to_lowercase();
        self.groups
            .get(self.tab)
            .map(|(_, rows)| rows.iter().filter(|r| r.search.contains(&term)).collect())
            .unwrap_or_default()
    }

    fn highlighted(&self) -> Option<&HistoryRow> {
        self.items().get(self.selected).copied()
    }

    fn is_checked(&self, row: &HistoryRow) -> bool {
        self.selected_targets.contains(&row.target)
    }

    /// The rows Enter would absorb: the checked ones, else the highlighted one.
    fn absorb_rows(&self) -> Vec<&HistoryRow> {
        if self.selected_targets.is_empty() {
            return self.highlighted().into_iter().collect();
        }
        self.groups
            .iter()
            .flat_map(|(_, rows)| rows)
            .filter(|r| self.is_checked(r))
            .collect()
    }

    fn absorb(&self) -> WorkspaceAction {
        let sources: Vec<Value> = self
            .absorb_rows()
            .iter()
            .map(|r| absorb_source(&r.target))
            .collect();
        if sources.is_empty() {
            return WorkspaceAction::None;
        }
        WorkspaceAction::Absorb {
            sources,
            focus: self.focus.clone(),
        }
    }

    /// `2 chats, ~2.9k tok` for what Enter would absorb.
    fn absorb_summary(&self) -> String {
        let rows = self.absorb_rows();
        let tokens: u64 = rows.iter().map(|r| r.packet_tokens()).sum();
        let plural = if rows.len() == 1 { "" } else { "s" };
        format!(
            "{} chat{plural}, ~{} tok",
            rows.len(),
            compact_count(tokens)
        )
    }

    fn toggle_highlighted(&mut self) {
        let Some(target) = self.highlighted().map(|r| r.target.clone()) else {
            return;
        };
        match self.selected_targets.iter().position(|t| *t == target) {
            Some(i) => {
                self.selected_targets.remove(i);
            }
            None => self.selected_targets.push(target),
        }
    }
}

// Machine editor

#[derive(Debug, Clone)]
struct MachineRow {
    name: String,
    source: String,
    kind: String,
    editable: bool,
}

#[derive(Debug, Clone, Default)]
struct DraftState {
    id: String,
    kind: String,
    role: String,
    model: String,
    raw_model: String,
    thinking: String,
    editable: bool,
}

/// A state's model with its thinking level beside it (`claude-opus-5-5 · medium`).
fn model_badge(model: &str, thinking: &str) -> String {
    // A role row's label can already carry it (`coder (anthropic/x · medium)`).
    if thinking.is_empty() || model.is_empty() || model.contains(&format!("\u{b7} {thinking}")) {
        model.to_owned()
    } else {
        format!("{model} \u{b7} {thinking}")
    }
}

/// The draft's graph with each state's resolved thinking level, for the diagram's model badges.
fn graph_with_thinking(mut graph: Value, states: &[DraftState]) -> Value {
    let Some(rows) = graph.get_mut("states").and_then(Value::as_array_mut) else {
        return graph;
    };
    for row in rows {
        let Some(state) = states
            .iter()
            .find(|state| row.get("id").and_then(Value::as_str) == Some(state.id.as_str()))
        else {
            continue;
        };
        if state.thinking.is_empty() {
            continue;
        }
        if let Some(subagent) = row.get_mut("subagent").and_then(Value::as_object_mut) {
            subagent
                .entry("thinking")
                .or_insert_with(|| json!(state.thinking));
        }
    }
    graph
}

#[derive(Debug, Clone, Default)]
struct Draft {
    dir: String,
    file: String,
    name: String,
    target: String,
    description: String,
    params: Vec<Value>,
    roles: Value,
    state_details: Value,
    /// The project root of a project factory draft; none for a library machine.
    project: Option<String>,
    states: Vec<DraftState>,
    selected: usize,
    /// The compiled spec for the graph view, or why it does not compile.
    graph: Option<Value>,
    graph_error: Option<String>,
    /// Scroll offset when viewing the interactive block diagram.
    graph_scroll: usize,
    /// True after manual PageUp/PageDown until the next node selection step.
    graph_manual_scroll: bool,
    /// Open node detail/files view: `(state_id, scroll_offset)`.
    node_detail: Option<(String, usize)>,
    /// The draft has edits that are not saved yet.
    dirty: bool,
    /// Esc on a dirty draft asked whether to save or discard.
    confirm_close: bool,
}

#[derive(Debug)]
enum MachineStep {
    List,
    Draft(Draft),
}

/// The text step the prompt editor answers.
#[derive(Debug, Clone, PartialEq)]
enum ComposeStep {
    /// The name for an editable copy of this library machine.
    CopyName(String),
    /// The whole definition file of the open draft.
    File,
}

#[derive(Debug)]
struct Machines {
    rows: Vec<MachineRow>,
    selected: usize,
    /// A machine named when the editor opened (`/fsm edit NAME`, factory `e`).
    preselect: Option<String>,
    /// A project factory opened from the Captain's review page as (root, name): the page edits
    /// only that factory and closes back to the review page.
    project: Option<(String, String)>,
    step: MachineStep,
    compose: Option<ComposeStep>,
    /// The draft shows the state graph instead of the state table; `None` follows the width
    /// (the graph from [`GRAPH_MIN_WIDTH`] columns).
    graph: Option<bool>,
    /// The pending discard drops unsaved edits, so it reports itself.
    discard_reports: bool,
}

/// The narrowest terminal the machine editor opens a draft on the graph view.
const GRAPH_MIN_WIDTH: usize = 80;

impl Machines {
    fn graph_on(&self, width: usize) -> bool {
        self.graph.unwrap_or(width >= GRAPH_MIN_WIDTH)
    }
}

const UNSAVED_PROMPT: &str = "Unsaved changes · s save · d discard · Esc keep editing";

const MERGE_PROMPT: &str = "Merge crew worktree into main branch? y merge · n cancel";

const ROLLBACK_PROMPT: &str = "Rollback worktree to last pre-turn checkpoint? (y/n)";

// Review

#[derive(Debug, Clone)]
struct DiffSide {
    number: i64,
    sign: String,
    text: String,
}

#[derive(Debug, Clone)]
enum DiffRow {
    Header(String),
    Hunk(String),
    Gap,
    Line {
        kind: String,
        old: Option<DiffSide>,
        new: Option<DiffSide>,
    },
}

#[derive(Debug, Default)]
struct Review {
    /// A Captain crew thread's checkout as (project, member), or none for this chat's checkout.
    member: Option<(String, String)>,
    root: String,
    staged: bool,
    files: Vec<(String, String)>,
    selected: usize,
    rows: Vec<DiffRow>,
    top: usize,
    column: usize,
    unified: bool,
    /// `M` on a crew review asked whether to merge its worktree into the main branch.
    confirm_merge: bool,
    /// `R` on a crew review asked whether to roll the worktree back to its last checkpoint.
    confirm_rollback: bool,
    findings: Vec<ReviewFinding>,
    /// `f` opened the findings pane, which takes the arrow keys.
    findings_focus: bool,
    /// The selected finding, as an index into [`Review::visible_findings`].
    finding_selected: usize,
    /// `a` shows resolved and irrelevant findings too.
    findings_all: bool,
}

/// One structured review finding and its triage status (`open`, `resolved`, `irrelevant`).
#[derive(Debug, Clone, Default, PartialEq)]
struct ReviewFinding {
    id: String,
    file: String,
    snippet_hash: String,
    category: String,
    severity: String,
    confidence: f64,
    line_hint: Option<i64>,
    message: String,
    status: String,
}

impl ReviewFinding {
    fn from_json(value: &Value) -> Option<Self> {
        let text = |key: &str| clean(value[key].as_str().unwrap_or_default());
        let id = text("id");
        if id.is_empty() {
            return None;
        }
        let status = match value["status"].as_str() {
            Some(s @ ("resolved" | "irrelevant")) => s.to_owned(),
            _ => "open".to_owned(),
        };
        Some(Self {
            id,
            file: text("file"),
            snippet_hash: text("snippet_hash"),
            category: text("category"),
            severity: text("severity").to_lowercase(),
            confidence: value["confidence"].as_f64().unwrap_or(0.0),
            line_hint: value["line_hint"].as_i64(),
            message: text("message"),
            status,
        })
    }

    fn severity_badge(&self) -> (&'static str, ThemeColor) {
        match self.severity.as_str() {
            "high" | "critical" | "major" => ("High", ThemeColor::Error),
            "med" | "medium" | "moderate" => ("Med", ThemeColor::Warning),
            _ => ("Low", ThemeColor::Dim),
        }
    }

    fn location(&self) -> String {
        match self.line_hint {
            Some(line) => format!("{}:{line}", self.file),
            None => self.file.clone(),
        }
    }
}

impl Review {
    /// One Captain backend action on this crew review, or none outside a crew review.
    fn captain_call(&self, request: WorkspaceRequest, act: &str, body: Value) -> WorkspaceAction {
        match &self.member {
            Some(_) => call(request, &["captain", "act", act], body),
            None => WorkspaceAction::None,
        }
    }

    fn findings_call(&self) -> WorkspaceAction {
        let Some((project, member)) = &self.member else {
            return WorkspaceAction::None;
        };
        self.captain_call(
            WorkspaceRequest::Findings,
            "review-findings",
            json!({"project": project, "member": member}),
        )
    }

    fn open_findings(&self) -> usize {
        self.findings.iter().filter(|f| f.status == "open").count()
    }

    fn visible_findings(&self) -> Vec<usize> {
        (0..self.findings.len())
            .filter(|&i| self.findings_all || self.findings[i].status == "open")
            .collect()
    }

    /// Clamp the finding selection and move the file list to its file.
    fn follow_finding(&mut self) -> WorkspaceAction {
        let visible = self.visible_findings();
        self.finding_selected = self.finding_selected.min(visible.len().saturating_sub(1));
        let Some(&i) = visible.get(self.finding_selected) else {
            return WorkspaceAction::None;
        };
        let file = &self.findings[i].file;
        match self.files.iter().position(|(path, _)| path == file) {
            Some(pos) if pos != self.selected => {
                self.selected = pos;
                self.top = 0;
                self.diff_call()
            }
            _ => WorkspaceAction::None,
        }
    }

    fn triage(&mut self, status: &str) -> WorkspaceAction {
        let visible = self.visible_findings();
        let Some(&i) = visible.get(self.finding_selected) else {
            return WorkspaceAction::None;
        };
        self.findings[i].status = status.to_owned();
        let id = self.findings[i].id.clone();
        let project = self
            .member
            .as_ref()
            .map(|m| m.0.clone())
            .unwrap_or_default();
        let action = self.captain_call(
            WorkspaceRequest::Triage,
            "review-triage",
            json!({"project": project, "id": id, "status": status}),
        );
        let follow = self.follow_finding();
        if matches!(action, WorkspaceAction::None) {
            follow
        } else {
            action
        }
    }

    fn findings_key(&mut self, key: &str, kb: &KeybindingsManager) -> Option<WorkspaceAction> {
        let action = match key {
            "o" => self.triage("open"),
            "r" => self.triage("resolved"),
            "i" => self.triage("irrelevant"),
            "a" => {
                self.findings_all = !self.findings_all;
                self.follow_finding()
            }
            _ if kb.matches(key, "tui.select.up") || key == "k" => {
                self.finding_selected = self.finding_selected.saturating_sub(1);
                self.follow_finding()
            }
            _ if kb.matches(key, "tui.select.down") || key == "j" => {
                self.finding_selected += 1;
                self.follow_finding()
            }
            _ => return None,
        };
        Some(action)
    }
    /// One review request: `pa thread review json` here, or through the Captain backend for a
    /// crew checkout (it resolves the thread's checkout in the Captain's thread store).
    fn request(&self, request: WorkspaceRequest, body: Value) -> WorkspaceAction {
        match &self.member {
            Some((project, member)) => call(
                request,
                &["captain", "act", "review"],
                json!({"project": project, "member": member, "request": body}),
            ),
            None => call(request, &["review", "json"], body),
        }
    }

    fn files_call(&self, staged: Option<bool>) -> WorkspaceAction {
        let mut input = json!({"op": "files"});
        if let Some(staged) = staged {
            input["staged"] = json!(staged);
        }
        self.request(WorkspaceRequest::Files, input)
    }

    fn diff_call(&self) -> WorkspaceAction {
        match self.files.get(self.selected) {
            Some((path, _)) => self.request(
                WorkspaceRequest::Diff,
                json!({"op": "diff", "path": path, "staged": self.staged,
                       "mode": if self.unified { "unified" } else { "split" }}),
            ),
            None => WorkspaceAction::None,
        }
    }
}

enum Page {
    History(History),
    Machines(Machines),
    Review(Review),
}

/// The open workspace page.
pub(crate) struct WorkspaceView {
    page: Page,
    viewport_rows: usize,
    loading: bool,
    notice: Option<String>,
    error: Option<String>,
    /// The chat's model (`provider/model`): factory steps that name no model or role run on it.
    host_model: String,
    /// The last rendered width: the machine editor's default view follows it.
    width: AtomicUsize,
}

impl WorkspaceView {
    fn with(page: Page) -> Self {
        Self {
            page,
            viewport_rows: 24,
            loading: true,
            notice: None,
            error: None,
            host_model: String::new(),
            width: AtomicUsize::new(GRAPH_MIN_WIDTH),
        }
    }

    /// The model of the chat that opened this page; diagrams show it for inheriting steps.
    pub(crate) fn set_host_model(&mut self, model: String) {
        self.host_model = model;
    }

    /// The history page and its first call; `query` prefills the search.
    pub(crate) fn history(query: &str) -> (Self, WorkspaceAction) {
        Self::history_page(query, false, None)
    }

    /// The history page in absorb mode for `/absorb [text] [--focus <topic>]`.
    pub(crate) fn absorb(args: &str) -> (Self, WorkspaceAction) {
        let (query, focus) = match args.split_once("--focus") {
            Some((query, focus)) => (query, Some(focus.trim()).filter(|f| !f.is_empty())),
            None => (args, None),
        };
        Self::history_page(query, true, focus.map(str::to_owned))
    }

    fn history_page(
        query: &str,
        absorb_mode: bool,
        focus: Option<String>,
    ) -> (Self, WorkspaceAction) {
        let mut history = History {
            groups: Vec::new(),
            tab: 0,
            selected: 0,
            search: SearchInput::new(),
            absorb_mode,
            selected_targets: Vec::new(),
            focus,
            composing_focus: false,
        };
        history.search.prefill(query.trim());
        (
            Self::with(Page::History(history)),
            call(
                WorkspaceRequest::HistoryList,
                &["history", "--json"],
                json!({}),
            ),
        )
    }

    /// The machine editor and its first call; `name` opens that machine.
    pub(crate) fn machines(name: &str) -> (Self, WorkspaceAction) {
        let machines = Machines {
            rows: Vec::new(),
            selected: 0,
            preselect: (!name.trim().is_empty()).then(|| name.trim().to_owned()),
            project: None,
            step: MachineStep::List,
            compose: None,
            graph: None,
            discard_reports: false,
        };
        (
            Self::with(Page::Machines(machines)),
            call(WorkspaceRequest::Machines, &["fsm-edit", "list"], json!({})),
        )
    }

    /// The machine editor on one project factory (`<root>/.prime/machines/<name>/`), opened
    /// from the Captain's factory review page. A saved edit changes the factory's digest, so
    /// the Captain asks for approval again.
    pub(crate) fn project_machine(root: &str, name: &str) -> (Self, WorkspaceAction) {
        let machines = Machines {
            rows: Vec::new(),
            selected: 0,
            preselect: None,
            project: Some((root.to_owned(), name.to_owned())),
            step: MachineStep::List,
            compose: None,
            graph: None,
            discard_reports: false,
        };
        (
            Self::with(Page::Machines(machines)),
            call(
                WorkspaceRequest::Draft,
                &["fsm-edit", "draft"],
                json!({"name": name, "project": root}),
            ),
        )
    }

    /// Whether closing this page returns to the Captain's factory review page.
    /// Whether closing this page returns to the Captain: its factory review page, or the crew
    /// row a crew review was opened from.
    pub(crate) fn returns_to_captain(&self) -> bool {
        match &self.page {
            Page::Machines(m) => m.project.is_some(),
            Page::Review(r) => r.member.is_some(),
            Page::History(_) => false,
        }
    }

    /// Whether closing this page may have changed a Captain project factory.
    pub(crate) fn edits_captain_factory(&self) -> bool {
        matches!(&self.page, Page::Machines(m) if m.project.is_some())
    }

    /// The last notice, for the page that opens after this one closes.
    pub(crate) fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }

    /// The review page for this checkout, or for a Captain crew thread's checkout
    /// given as (project, member).
    pub(crate) fn review(member: Option<(String, String)>) -> (Self, WorkspaceAction) {
        let review = Review {
            member,
            ..Review::default()
        };
        let action = review.files_call(None);
        (Self::with(Page::Review(review)), action)
    }

    /// A crew review's background findings load, sent beside its first call.
    pub(crate) fn findings_call(&self) -> WorkspaceAction {
        match &self.page {
            Page::Review(r) => r.findings_call(),
            _ => WorkspaceAction::None,
        }
    }

    pub(crate) fn set_viewport_rows(&mut self, rows: usize) {
        self.viewport_rows = rows.max(10);
    }

    /// The `$EDITOR` handoff returned: re-read the draft.
    pub(crate) fn file_edited(&mut self) -> WorkspaceAction {
        if let Page::Machines(Machines {
            step: MachineStep::Draft(draft),
            ..
        }) = &mut self.page
        {
            draft.dirty = true;
            self.loading = true;
            return call(
                WorkspaceRequest::States,
                &["fsm-edit", "states"],
                json!({"draft": draft.dir}),
            );
        }
        WorkspaceAction::None
    }

    pub(crate) fn handle_key(&mut self, key: &str, kb: &KeybindingsManager) -> WorkspaceAction {
        if key == "ctrl+c" {
            return self.leave();
        }
        self.notice = None;
        self.error = None;
        match &mut self.page {
            Page::History(history) => Self::history_key(history, key, kb),
            Page::Machines(machines) => {
                let graph = machines.graph_on(self.width.load(Ordering::Relaxed));
                let action = Self::machines_key(machines, key, kb, graph, &mut self.notice);
                if matches!(action, WorkspaceAction::Call { .. }) {
                    self.loading = true;
                }
                action
            }
            Page::Review(review) => {
                let action = Self::review_key(review, key, kb, self.viewport_rows);
                if matches!(action, WorkspaceAction::MergeWorktree(_))
                    || matches!(
                        action,
                        WorkspaceAction::Call {
                            request: WorkspaceRequest::Rollback,
                            ..
                        }
                    )
                {
                    self.loading = true;
                }
                action
            }
        }
    }

    /// One bracketed paste: the history search takes it; other pages ignore it.
    pub(crate) fn paste(&mut self, text: &str) {
        if let Page::History(h) = &mut self.page {
            h.search.paste(&clean(text));
            h.selected = 0;
        }
    }

    /// The prompt editor's answer to a [`WorkspaceAction::Compose`]: `None` when cancelled.
    pub(crate) fn compose_done(&mut self, text: Option<String>) -> WorkspaceAction {
        if let Page::History(h) = &mut self.page {
            if std::mem::take(&mut h.composing_focus) {
                if let Some(text) = text {
                    h.focus = Some(clean(text.trim())).filter(|f| !f.is_empty());
                }
            }
            return WorkspaceAction::None;
        }
        let Page::Machines(m) = &mut self.page else {
            return WorkspaceAction::None;
        };
        let Some(step) = m.compose.take() else {
            return WorkspaceAction::None;
        };
        let Some(text) = text else {
            return WorkspaceAction::None;
        };
        let action = match (step, &m.step) {
            (ComposeStep::CopyName(name), _) => call(
                WorkspaceRequest::Draft,
                &["fsm-edit", "draft"],
                json!({"name": name, "target": text.trim()}),
            ),
            (ComposeStep::File, MachineStep::Draft(d)) => call(
                WorkspaceRequest::Write,
                &["fsm-edit", "write"],
                json!({"draft": d.dir, "text": text}),
            ),
            _ => WorkspaceAction::None,
        };
        self.loading = matches!(action, WorkspaceAction::Call { .. });
        action
    }

    /// Apply a model and thinking level returned by pa's shared model picker.
    pub(crate) fn apply_picked_model(
        &mut self,
        draft: String,
        state_id: String,
        model: String,
        thinking: String,
        edit_role: bool,
    ) -> WorkspaceAction {
        let mut input = json!({
            "draft": draft,
            "state": state_id,
            "model": model,
            "thinking": thinking,
        });
        if edit_role {
            input["edit_role"] = json!(true);
        }
        self.loading = true;
        call(
            WorkspaceRequest::SetModel,
            &["fsm-edit", "set-model"],
            input,
        )
    }

    /// Close the page; an open machine draft is discarded first.
    fn leave(&mut self) -> WorkspaceAction {
        if let Page::Machines(m) = &mut self.page {
            if let MachineStep::Draft(d) = &m.step {
                let draft = d.dir.clone();
                m.step = MachineStep::List;
                return call(
                    WorkspaceRequest::Discard,
                    &["fsm-edit", "discard"],
                    json!({"draft": draft}),
                );
            }
        }
        WorkspaceAction::Close
    }

    fn history_key(h: &mut History, key: &str, kb: &KeybindingsManager) -> WorkspaceAction {
        let count = h.items().len();
        let tabs = h.groups.len().max(1);
        let empty = h.search.value().is_empty();
        match key {
            "escape" if !empty => h.search = SearchInput::new(),
            "escape" => return WorkspaceAction::Close,
            "tab" => h.tab = (h.tab + 1) % tabs,
            "shift+tab" => h.tab = (h.tab + tabs - 1) % tabs,
            "right" if empty => h.tab = (h.tab + 1) % tabs,
            "left" if empty => h.tab = (h.tab + tabs - 1) % tabs,
            "space" | " " if h.absorb_mode || empty => {
                h.toggle_highlighted();
                return WorkspaceAction::None;
            }
            "f" if empty => {
                h.composing_focus = true;
                return WorkspaceAction::Compose {
                    label: "focus topic for the absorbed context".into(),
                    initial: h.focus.clone().unwrap_or_default(),
                };
            }
            "a" if empty && !h.absorb_mode && h.highlighted().is_some_and(HistoryRow::is_prime) => {
                return h.absorb();
            }
            "enter" => {
                let Some(row) = h.highlighted() else {
                    return WorkspaceAction::None;
                };
                if h.absorb_mode || !h.selected_targets.is_empty() || !row.is_prime() {
                    return h.absorb();
                }
                return call(
                    WorkspaceRequest::HistoryOpen,
                    &["history", "--open"],
                    json!({"target": row.target, "title": row.title}),
                );
            }
            _ if empty && key.len() == 1 && key.chars().all(|c| ('1'..='9').contains(&c)) => {
                let index = key.parse::<usize>().unwrap_or(1) - 1;
                h.tab = index.min(tabs - 1);
            }
            _ if kb.matches(key, "tui.select.up") => {
                h.selected = h.selected.saturating_sub(1);
                return WorkspaceAction::None;
            }
            _ if kb.matches(key, "tui.select.down") => {
                h.selected = (h.selected + 1).min(count.saturating_sub(1));
                return WorkspaceAction::None;
            }
            _ if kb.matches(key, "tui.select.pageUp") => {
                h.selected = h.selected.saturating_sub(10);
                return WorkspaceAction::None;
            }
            _ if kb.matches(key, "tui.select.pageDown") => {
                h.selected = (h.selected + 10).min(count.saturating_sub(1));
                return WorkspaceAction::None;
            }
            _ => {
                // Every other key edits the search, as in the settings menu.
                let before = h.search.value().to_owned();
                h.search.handle_key(key, kb);
                if h.search.value() == before {
                    return WorkspaceAction::None;
                }
            }
        }
        h.selected = 0;
        WorkspaceAction::None
    }

    fn machines_key(
        m: &mut Machines,
        key: &str,
        kb: &KeybindingsManager,
        graph: bool,
        notice: &mut Option<String>,
    ) -> WorkspaceAction {
        let up = kb.matches(key, "tui.select.up") || key == "k";
        let down = kb.matches(key, "tui.select.down") || key == "j";
        let project = m.project.is_some();
        match &mut m.step {
            MachineStep::List => match key {
                "q" | "escape" => WorkspaceAction::Close,
                "enter" => Self::open_machine(m),
                _ if up => {
                    m.selected = m.selected.saturating_sub(1);
                    WorkspaceAction::None
                }
                _ if down => {
                    m.selected = (m.selected + 1).min(m.rows.len().saturating_sub(1));
                    WorkspaceAction::None
                }
                _ => WorkspaceAction::None,
            },
            MachineStep::Draft(d) => {
                if let Some((_, ref mut detail_scroll)) = d.node_detail {
                    let page = 10usize;
                    if key == "escape" || key == "q" || key == "left" {
                        d.node_detail = None;
                    } else if up {
                        *detail_scroll = detail_scroll.saturating_sub(1);
                    } else if down {
                        *detail_scroll += 1;
                    } else if key == "pageup" || kb.matches(key, "tui.select.pageUp") {
                        *detail_scroll = detail_scroll.saturating_sub(page);
                    } else if key == "pagedown" || kb.matches(key, "tui.select.pageDown") {
                        *detail_scroll += page;
                    }
                    return WorkspaceAction::None;
                }
                if d.confirm_close {
                    d.confirm_close = false;
                    return match key {
                        "s" => call(
                            WorkspaceRequest::Save,
                            &["fsm-edit", "save"],
                            json!({"draft": d.dir}),
                        ),
                        "d" => {
                            let input = json!({"draft": d.dir});
                            m.step = MachineStep::List;
                            m.discard_reports = true;
                            call(WorkspaceRequest::Discard, &["fsm-edit", "discard"], input)
                        }
                        _ => WorkspaceAction::None,
                    };
                }
                match key {
                    "escape" | "q" if d.dirty => {
                        d.confirm_close = true;
                        *notice = Some(UNSAVED_PROMPT.into());
                        WorkspaceAction::None
                    }
                    "escape" | "q" => {
                        let input = json!({"draft": d.dir});
                        m.step = MachineStep::List;
                        m.discard_reports = false;
                        call(WorkspaceRequest::Discard, &["fsm-edit", "discard"], input)
                    }
                    "e" => call(
                        WorkspaceRequest::Read,
                        &["fsm-edit", "read"],
                        json!({"draft": d.dir}),
                    ),
                    // The optional outside editor stays for library machines only.
                    "E" | "shift+e" if !project => WorkspaceAction::EditFile(d.file.clone()),
                    "g" => {
                        m.graph = Some(!graph);
                        d.graph_scroll = 0;
                        d.graph_manual_scroll = false;
                        WorkspaceAction::None
                    }
                    "s" => call(
                        WorkspaceRequest::Save,
                        &["fsm-edit", "save"],
                        json!({"draft": d.dir}),
                    ),
                    "r" => call(
                        WorkspaceRequest::States,
                        &["fsm-edit", "states"],
                        json!({"draft": d.dir}),
                    ),
                    "enter" => {
                        if let Some(state) = d.states.get(d.selected) {
                            d.node_detail = Some((state.id.clone(), 0));
                        }
                        WorkspaceAction::None
                    }
                    "m" => {
                        if let Some(state) = d.states.get(d.selected) {
                            if state.editable {
                                return WorkspaceAction::PickModel {
                                    draft: d.dir.clone(),
                                    state_id: state.id.clone(),
                                    role: (!state.role.is_empty()).then(|| state.role.clone()),
                                    current_model: if !state.raw_model.is_empty() {
                                        state.raw_model.clone()
                                    } else if state.model == "inherit" {
                                        String::new()
                                    } else {
                                        state.model.clone()
                                    },
                                    current_thinking: state.thinking.clone(),
                                };
                            }
                            *notice = Some(if state.kind == "python" || state.model == "python" {
                                "Python state has no model.".into()
                            } else {
                                "This state uses a referenced agent. Press e to edit the definition."
                                    .into()
                            });
                        }
                        WorkspaceAction::None
                    }
                    "pageup" if graph => {
                        d.graph_manual_scroll = true;
                        d.graph_scroll = d.graph_scroll.saturating_sub(10);
                        WorkspaceAction::None
                    }
                    "pagedown" if graph => {
                        d.graph_manual_scroll = true;
                        d.graph_scroll += 10;
                        WorkspaceAction::None
                    }
                    _ if up || down => {
                        let delta: isize = if up { -1 } else { 1 };
                        if graph {
                            let theme = Theme::builtin("prime", crate::theme::ColorMode::TrueColor);
                            if let Some(diagram) = d.graph.as_ref().and_then(|spec| {
                                crate::factory_view::machine_block_diagram(
                                    &theme,
                                    spec,
                                    &crate::factory_view::DiagramRenderOptions::default(),
                                )
                            }) {
                                if !diagram.node_spans.is_empty() {
                                    let cur_id = d.states.get(d.selected).map(|s| s.id.as_str());
                                    let cur = cur_id.and_then(|id| {
                                        diagram.node_spans.iter().position(|(s, _, _)| s == id)
                                    });
                                    let next = match cur {
                                        Some(i) => (i as isize + delta)
                                            .clamp(0, diagram.node_spans.len() as isize - 1)
                                            as usize,
                                        None => 0,
                                    };
                                    let (ref id, top, bottom) = diagram.node_spans[next];
                                    if let Some(pos) = d.states.iter().position(|s| s.id == *id) {
                                        d.selected = pos;
                                    }
                                    d.graph_manual_scroll = false;
                                    d.graph_scroll = crate::factory_view::scroll_to_span(
                                        d.graph_scroll,
                                        top,
                                        bottom,
                                        12,
                                        diagram.lines.len(),
                                    );
                                    return WorkspaceAction::None;
                                }
                            }
                        }
                        d.selected = (d.selected as isize + delta)
                            .clamp(0, d.states.len().saturating_sub(1) as isize)
                            as usize;
                        WorkspaceAction::None
                    }
                    _ => WorkspaceAction::None,
                }
            }
        }
    }

    /// Enter on a machine: draft it, or first ask for the copy's name.
    fn open_machine(m: &mut Machines) -> WorkspaceAction {
        let Some(row) = m.rows.get(m.selected) else {
            return WorkspaceAction::None;
        };
        if row.editable {
            return call(
                WorkspaceRequest::Draft,
                &["fsm-edit", "draft"],
                json!({"name": row.name}),
            );
        }
        m.compose = Some(ComposeStep::CopyName(row.name.clone()));
        WorkspaceAction::Compose {
            label: format!("name for your editable copy of {}", row.name),
            initial: format!("my-{}", row.name),
        }
    }

    fn review_key(
        r: &mut Review,
        key: &str,
        kb: &KeybindingsManager,
        rows: usize,
    ) -> WorkspaceAction {
        let page = rows.saturating_sub(10).max(1);
        let max = r.rows.len().saturating_sub(1);
        if r.confirm_merge {
            r.confirm_merge = false;
            return match key {
                "y" | "enter" if !r.root.is_empty() => {
                    WorkspaceAction::MergeWorktree(r.root.clone())
                }
                _ => WorkspaceAction::None,
            };
        }
        if r.confirm_rollback {
            r.confirm_rollback = false;
            return match key {
                "y" => match r.member.clone() {
                    Some((project, member)) => r.captain_call(
                        WorkspaceRequest::Rollback,
                        "checkpoint-rollback",
                        json!({"project": project, "member": member}),
                    ),
                    None => WorkspaceAction::None,
                },
                _ => WorkspaceAction::None,
            };
        }
        if r.findings_focus {
            if let Some(action) = r.findings_key(key, kb) {
                return action;
            }
        }
        match key {
            "q" | "escape" => return WorkspaceAction::Close,
            "M" | "shift+m" if r.member.is_some() => r.confirm_merge = true,
            "R" | "shift+r" if r.member.is_some() => r.confirm_rollback = true,
            "f" if !r.findings.is_empty() => {
                r.findings_focus = !r.findings_focus;
                if r.findings_focus {
                    return r.follow_finding();
                }
            }
            "tab" => return r.files_call(Some(!r.staged)),
            "r" => return r.files_call(Some(r.staged)),
            "v" => {
                r.unified = !r.unified;
                return r.diff_call();
            }
            "s" => {
                if let Some((path, _)) = r.files.get(r.selected) {
                    let op = if r.staged { "unstage" } else { "stage" };
                    return r.request(WorkspaceRequest::Stage, json!({"op": op, "path": path}));
                }
            }
            "right" => r.column += 8,
            "left" => r.column = r.column.saturating_sub(8),
            "home" => r.column = 0,
            "J" => r.top = (r.top + 1).min(max),
            "K" => r.top = r.top.saturating_sub(1),
            "n" | "p" => {
                let mut hunks = r
                    .rows
                    .iter()
                    .enumerate()
                    .filter(|(_, row)| matches!(row, DiffRow::Hunk(_)))
                    .map(|(i, _)| i);
                let next = if key == "n" {
                    hunks.find(|i| *i > r.top)
                } else {
                    hunks.rfind(|i| *i < r.top)
                };
                if let Some(i) = next {
                    r.top = i;
                }
            }
            _ if kb.matches(key, "tui.select.pageDown") => r.top = (r.top + page).min(max),
            _ if kb.matches(key, "tui.select.pageUp") => r.top = r.top.saturating_sub(page),
            _ if kb.matches(key, "tui.select.up") || key == "k" => {
                if r.selected > 0 {
                    r.selected -= 1;
                    return r.diff_call();
                }
            }
            _ if kb.matches(key, "tui.select.down") || key == "j" => {
                if r.selected + 1 < r.files.len() {
                    r.selected += 1;
                    return r.diff_call();
                }
            }
            _ => {}
        }
        WorkspaceAction::None
    }

    /// One background call's answer.
    pub(crate) fn apply(
        &mut self,
        request: WorkspaceRequest,
        result: Result<Value, String>,
    ) -> WorkspaceAction {
        if request == WorkspaceRequest::Findings {
            // Best effort beside the file list: no spinner change, and no error when the
            // backend has no findings support.
            if let (Page::Review(r), Ok(data)) = (&mut self.page, &result) {
                let mut seen = std::collections::HashSet::new();
                r.findings = data["findings"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(ReviewFinding::from_json)
                    .filter(|f| f.snippet_hash.is_empty() || seen.insert(f.snippet_hash.clone()))
                    .collect();
                r.finding_selected = 0;
                if r.findings.is_empty() {
                    r.findings_focus = false;
                }
            }
            return WorkspaceAction::None;
        }
        if request == WorkspaceRequest::Triage && result.is_ok() {
            return WorkspaceAction::None;
        }
        self.loading = false;
        let data = match result {
            Ok(data) => data,
            Err(error) => {
                self.error = Some(error);
                return WorkspaceAction::None;
            }
        };
        match (&mut self.page, request) {
            (Page::History(h), WorkspaceRequest::HistoryList) => {
                h.groups = data["groups"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|g| {
                        let rows = g["rows"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|r| HistoryRow {
                                title: clean(r["title"].as_str().unwrap_or("Untitled thread")),
                                search: r["search"].as_str().unwrap_or_default().to_lowercase(),
                                target: r["target"].clone(),
                                raw_bytes: r["rawBytes"].as_u64().unwrap_or(0),
                                cached_tokens: r["cachedTokens"].as_u64(),
                            })
                            .collect();
                        (clean(g["name"].as_str().unwrap_or("")), rows)
                    })
                    .collect();
                h.tab = h.tab.min(h.groups.len().saturating_sub(1));
            }
            (Page::History(_), WorkspaceRequest::HistoryOpen) => {
                if let Some(path) = data["path"].as_str() {
                    return WorkspaceAction::Resume(path.to_owned());
                }
            }
            (Page::Machines(m), WorkspaceRequest::Machines) => {
                m.rows = data["machines"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|r| MachineRow {
                        name: clean(r["name"].as_str().unwrap_or("")),
                        source: clean(r["source"].as_str().unwrap_or("")),
                        kind: clean(r["kind"].as_str().unwrap_or("")),
                        editable: r["editable"].as_bool().unwrap_or(false),
                    })
                    .collect();
                m.selected = m.selected.min(m.rows.len().saturating_sub(1));
                if let Some(name) = m.preselect.take() {
                    match m.rows.iter().position(|r| r.name == name) {
                        Some(index) => {
                            m.selected = index;
                            let action = Self::open_machine(m);
                            self.loading = matches!(action, WorkspaceAction::Call { .. });
                            return action;
                        }
                        None => {
                            self.notice = Some(format!(
                                "{name} has no library definition. Choose a library machine to edit."
                            ));
                        }
                    }
                }
            }
            (Page::Machines(m), WorkspaceRequest::Read) => {
                if let MachineStep::Draft(d) = &m.step {
                    m.compose = Some(ComposeStep::File);
                    let file = std::path::Path::new(&d.file)
                        .file_name()
                        .map_or_else(|| d.file.clone(), |f| f.to_string_lossy().into_owned());
                    return WorkspaceAction::Compose {
                        label: format!("{} · {file}", d.target),
                        initial: data["text"].as_str().unwrap_or_default().to_owned(),
                    };
                }
            }
            (
                Page::Machines(m),
                WorkspaceRequest::Draft
                | WorkspaceRequest::States
                | WorkspaceRequest::SetModel
                | WorkspaceRequest::Write,
            ) => {
                let (selected, graph_scroll, node_detail, dirty) = match &m.step {
                    MachineStep::Draft(d) => {
                        (d.selected, d.graph_scroll, d.node_detail.clone(), d.dirty)
                    }
                    MachineStep::List => (0, 0, None, false),
                };
                let dirty = dirty
                    || matches!(
                        request,
                        WorkspaceRequest::SetModel | WorkspaceRequest::Write
                    );
                let states: Vec<DraftState> = data["states"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|s| DraftState {
                        id: clean(s["id"].as_str().unwrap_or("")),
                        kind: clean(s["kind"].as_str().unwrap_or("")),
                        role: clean(s["role"].as_str().unwrap_or("")),
                        model: clean(s["model"].as_str().unwrap_or("")),
                        raw_model: clean(s["raw_model"].as_str().unwrap_or("")),
                        thinking: clean(s["thinking"].as_str().unwrap_or("")),
                        editable: s["editable"].as_bool().unwrap_or(false),
                    })
                    .collect();
                let graph = data
                    .get("graph")
                    .filter(|g| g.is_object())
                    .cloned()
                    .map(|graph| graph_with_thinking(graph, &states));
                m.step = MachineStep::Draft(Draft {
                    dir: data["draft"].as_str().unwrap_or_default().to_owned(),
                    file: data["file"].as_str().unwrap_or_default().to_owned(),
                    name: clean(data["name"].as_str().unwrap_or("")),
                    target: clean(data["target"].as_str().unwrap_or("")),
                    description: clean(data["description"].as_str().unwrap_or("")),
                    params: data["params"].as_array().cloned().unwrap_or_default(),
                    roles: data.get("roles").cloned().unwrap_or_else(|| json!({})),
                    state_details: data
                        .get("state_details")
                        .cloned()
                        .unwrap_or_else(|| json!({})),
                    project: data["project"].as_str().map(str::to_owned),
                    selected: selected.min(states.len().saturating_sub(1)),
                    states,
                    graph,
                    graph_error: data["graph_error"].as_str().map(clean),
                    graph_scroll,
                    graph_manual_scroll: false,
                    node_detail,
                    dirty,
                    confirm_close: false,
                });
                match request {
                    WorkspaceRequest::SetModel => {
                        self.notice = Some("Model updated in the draft. Press s to save.".into());
                    }
                    WorkspaceRequest::Write => {
                        self.notice = Some("Draft updated. Press s to validate and save.".into());
                    }
                    _ => {}
                }
            }
            (Page::Machines(m), WorkspaceRequest::Save) => {
                m.step = MachineStep::List;
                let saved = data["saved"].as_str().unwrap_or("the machine").to_owned();
                if m.project.is_some() {
                    self.notice = Some(format!(
                        "Saved {saved}. This version needs your approval before it runs."
                    ));
                    return WorkspaceAction::Close;
                }
                self.notice = Some(format!("Saved {saved} for future runs."));
                self.loading = true;
                return call(WorkspaceRequest::Machines, &["fsm-edit", "list"], json!({}));
            }
            (Page::Machines(m), WorkspaceRequest::Discard) => {
                m.step = MachineStep::List;
                if std::mem::take(&mut m.discard_reports) {
                    self.notice = Some("Draft discarded.".into());
                }
                if m.project.is_some() {
                    return WorkspaceAction::Close;
                }
            }
            (Page::Review(r), WorkspaceRequest::Files) => {
                let previous = r.files.get(r.selected).map(|f| f.0.clone());
                data["root"]
                    .as_str()
                    .unwrap_or_default()
                    .clone_into(&mut r.root);
                r.staged = data["staged"].as_bool().unwrap_or(false);
                r.files = data["files"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|f| {
                        (
                            f["path"].as_str().unwrap_or_default().to_owned(),
                            f["status"].as_str().unwrap_or(" ").to_owned(),
                        )
                    })
                    .collect();
                r.selected = previous
                    .and_then(|p| r.files.iter().position(|f| f.0 == p))
                    .unwrap_or(r.selected)
                    .min(r.files.len().saturating_sub(1));
                r.rows.clear();
                r.top = 0;
                r.column = 0;
                let action = r.diff_call();
                self.loading = matches!(action, WorkspaceAction::Call { .. });
                return action;
            }
            (Page::Review(r), WorkspaceRequest::Diff) => {
                r.rows = data["rows"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(diff_row)
                    .collect();
                r.top = r.top.min(r.rows.len().saturating_sub(1));
            }
            (Page::Review(r), WorkspaceRequest::Merge) => {
                self.notice = data["message"].as_str().map(str::to_owned);
                self.loading = true;
                return r.files_call(Some(r.staged));
            }
            (Page::Review(r), WorkspaceRequest::Rollback) => {
                self.notice = Some(
                    data["message"]
                        .as_str()
                        .map(clean)
                        .unwrap_or_else(|| "Rolled back to the last pre-turn checkpoint.".into()),
                );
                self.loading = true;
                return r.files_call(Some(r.staged));
            }
            (Page::Review(r), WorkspaceRequest::Stage) => {
                self.loading = true;
                return r.files_call(Some(r.staged));
            }
            _ => {}
        }
        WorkspaceAction::None
    }

    pub(crate) fn render(&self, theme: &Theme, width: usize, kb: &KeybindingsManager) -> Vec<Line> {
        let width = width.max(20);
        self.width.store(width, Ordering::Relaxed);
        let (title, summary, path) = self.header();
        let mut head = vec![
            Span::raw(" "),
            theme.bold(theme.fg_span(ThemeColor::Accent, title)),
            Span::raw("  "),
            theme.fg_span(ThemeColor::Muted, summary),
        ];
        if !path.is_empty() {
            head.push(theme.fg_span(ThemeColor::Dim, format!("  {path}")));
        }
        let mut out: Vec<Line> = vec![truncate_line(&head, width, "…")];
        // The history page opens on its search field, whose rules frame the header.
        if !matches!(self.page, Page::History(_)) {
            out.push(rule_row(theme, width));
        }
        let body_rows = self.viewport_rows.saturating_sub(out.len() + 3);
        let body = match &self.page {
            Page::History(h) => Self::render_history(h, theme, width, body_rows),
            Page::Machines(m) => {
                Self::render_machines(m, &self.host_model, theme, width, body_rows)
            }
            Page::Review(r) => Self::render_review(r, theme, width, body_rows),
        };
        out.extend(body.into_iter().take(body_rows));
        while out.len() < self.viewport_rows.saturating_sub(3) {
            out.push(Vec::new());
        }
        let status = self
            .error
            .as_ref()
            .map(|e| theme.fg_span(ThemeColor::Error, clean(e)))
            .or_else(|| {
                self.notice
                    .as_ref()
                    .map(|n| theme.fg_span(ThemeColor::Text, clean(n)))
            })
            .or_else(|| {
                self.loading
                    .then(|| theme.fg_span(ThemeColor::Dim, "Loading…"))
            })
            .unwrap_or_else(|| Span::raw(""));
        out.push(truncate_line(&vec![Span::raw(" "), status], width, "…"));
        out.push(rule_row(theme, width));
        out.push(hint_row(theme, width, &self.footer(width, kb)));
        out
    }

    fn header(&self) -> (&'static str, String, String) {
        match &self.page {
            Page::History(h) => {
                let total: usize = h.groups.iter().map(|g| g.1.len()).sum();
                let mut summary = format!("{total} conversations");
                if let Some(focus) = &h.focus {
                    summary.push_str(&format!(" · focus: {focus}"));
                }
                let title = if h.absorb_mode { "absorb" } else { "history" };
                (title, summary, String::new())
            }
            Page::Machines(m) => match &m.step {
                MachineStep::Draft(d) => match &m.project {
                    // The factory's own folder: the draft path is an implementation detail.
                    Some((root, name)) => (
                        "machines",
                        "project factory".into(),
                        format!("{root}/.prime/machines/{name}"),
                    ),
                    None => (
                        "machines",
                        format!("{} installed", m.rows.len()),
                        d.file.clone(),
                    ),
                },
                MachineStep::List => (
                    "machines",
                    format!("{} installed", m.rows.len()),
                    String::new(),
                ),
            },
            Page::Review(r) => (
                "review",
                format!(
                    "{} {} file{}{}",
                    r.files.len(),
                    if r.staged { "staged" } else { "unstaged" },
                    if r.files.len() == 1 { "" } else { "s" },
                    if r.findings.is_empty() {
                        String::new()
                    } else {
                        format!(" · Findings ({} open)", r.open_findings())
                    }
                ),
                r.root.clone(),
            ),
        }
    }

    fn footer(&self, width: usize, kb: &KeybindingsManager) -> String {
        let _ = kb;
        let narrow = width < 100;
        match &self.page {
            Page::History(h) => {
                let keys = if h.absorb_mode {
                    format!(
                        "Space select · f focus · Enter absorb ({}) · Tab provider · Esc close",
                        h.absorb_summary()
                    )
                } else if !h.selected_targets.is_empty() {
                    format!(
                        "Enter absorb ({}) · Space select · f focus · Tab provider · Esc",
                        h.absorb_summary()
                    )
                } else if h.highlighted().is_some_and(HistoryRow::is_prime) {
                    "Enter resume · a absorb · Space select · Tab provider · Esc".into()
                } else {
                    "Enter absorb · Space select · f focus · Tab provider · Esc".into()
                };
                if narrow {
                    keys
                } else {
                    format!("Type to search · ↑↓ · {keys}")
                }
            }
            Page::Machines(m) => match &m.step {
                MachineStep::List => "↑↓ select · Enter edit · Esc close".into(),
                MachineStep::Draft(d) => {
                    if d.node_detail.is_some() {
                        return "↑/↓ scroll · PageUp/PageDown page · Esc back".into();
                    }
                    if d.confirm_close {
                        return UNSAVED_PROMPT.into();
                    }
                    let graph_on = m.graph_on(width);
                    let graph = if graph_on { "g states" } else { "g graph" };
                    let outside = if m.project.is_some() {
                        ""
                    } else {
                        " · E $EDITOR"
                    };
                    let back = if m.project.is_some() || !d.dirty {
                        "Esc back"
                    } else {
                        "Esc discard"
                    };
                    if graph_on && narrow {
                        format!("↑/↓ node · Enter files · m model · {graph} · s save · {back}")
                    } else if graph_on {
                        format!("↑/↓ node · PageUp/PageDown scroll · Enter files · m model · {graph} · s save{outside} · {back}")
                    } else if narrow {
                        format!("↑↓ · Enter files · m model · e edit · {graph} · s save · {back}")
                    } else {
                        format!("↑↓ state · Enter files · m model · e edit file · {graph} · s validate and save · r reload{outside} · {back}")
                    }
                }
            },
            Page::Review(r) => {
                if r.confirm_merge {
                    return MERGE_PROMPT.into();
                }
                if r.confirm_rollback {
                    return ROLLBACK_PROMPT.into();
                }
                if r.findings_focus {
                    let all = if r.findings_all {
                        "a open only"
                    } else {
                        "a all"
                    };
                    return format!("↑↓ finding · o open · r resolved · i irrelevant · {all} · f diff · Esc close");
                }
                let stage = if r.staged { "s unstage" } else { "s stage" };
                let merge = match (r.member.is_some(), r.findings.is_empty()) {
                    (false, _) => "",
                    (true, true) => " · M merge · R rollback",
                    (true, false) => " · M merge · R rollback · f findings",
                };
                if narrow {
                    format!("↑↓ file · PgUp/PgDn · Tab staged · {stage}{merge} · Esc close")
                } else {
                    format!("↑↓ file · ←→ pan · PgUp/PgDn scroll · n/p hunk · Tab staged · v layout · {stage}{merge} · r refresh · Esc close")
                }
            }
        }
    }

    /// The rows `top..top+room` that keep `selected` visible.
    fn window(selected: usize, len: usize, room: usize) -> std::ops::Range<usize> {
        let room = room.max(1);
        let top = selected
            .saturating_sub(room - 1)
            .min(len.saturating_sub(room));
        top..(top + room).min(len)
    }

    fn render_history(h: &History, theme: &Theme, width: usize, rows: usize) -> Vec<Line> {
        // The settings menu's layout: search field, tab strip, rows.
        let mut out = crate::menu_panel::search_field_lines(
            theme,
            width,
            h.search.value(),
            h.search.cursor(),
            true,
            "Search threads",
        );
        out.push(Vec::new());
        let names: Vec<String> = h
            .groups
            .iter()
            .map(|(name, rows)| format!("{name} {}", rows.len()))
            .collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        out.push(crate::settings_menu::tabs::strip_row(
            theme, width, &names, h.tab,
        ));
        out.push(Vec::new());
        let items = h.items();
        if items.is_empty() {
            out.push(no_match_row(theme, width, "No matching threads"));
            return out;
        }
        let room = rows.saturating_sub(out.len());
        for i in Self::window(h.selected, items.len(), room) {
            let row = items[i];
            let mut label = Vec::new();
            if h.is_checked(row) {
                label.push(theme.fg_span(ThemeColor::Success, "[✓] "));
            } else if h.absorb_mode {
                label.push(theme.fg_span(ThemeColor::Dim, "[ ] "));
            }
            label.push(theme.fg_span(ThemeColor::Text, row.title.clone()));
            let cost = row.cost_preview();
            let trailing: Vec<MenuSegment> = cost.iter().map(|c| MenuSegment::muted(c)).collect();
            out.push(menu_row(theme, width, label, &trailing, i == h.selected));
        }
        out
    }

    fn render_machines(
        m: &Machines,
        host_model: &str,
        theme: &Theme,
        width: usize,
        rows: usize,
    ) -> Vec<Line> {
        let dim = |t: String| theme.fg_span(ThemeColor::Dim, t);
        match &m.step {
            MachineStep::List => {
                let mut out: Vec<Line> = Vec::new();
                if m.rows.is_empty() && m.project.is_none() {
                    out.push(no_match_row(theme, width, "No machines installed"));
                }
                for i in Self::window(m.selected, m.rows.len(), rows) {
                    let row = &m.rows[i];
                    let kind = if row.kind == "toml" {
                        "machine.toml"
                    } else {
                        "MACHINE.md"
                    };
                    let mut trailing =
                        vec![MenuSegment::muted(&row.source), MenuSegment::muted(kind)];
                    if !row.editable {
                        trailing.push(MenuSegment::muted("copy to edit"));
                    }
                    out.push(menu_row(
                        theme,
                        width,
                        vec![theme.fg_span(ThemeColor::Text, row.name.clone())],
                        &trailing,
                        i == m.selected,
                    ));
                }
                out
            }
            MachineStep::Draft(d) => {
                if let Some((state_id, detail_scroll)) = &d.node_detail {
                    let mut detail = d
                        .state_details
                        .get(state_id)
                        .cloned()
                        .unwrap_or_else(|| json!({"id": state_id}));
                    detail["inherit_model"] = json!(host_model);
                    let body = crate::factory_view::render_node_detail_lines(
                        theme, width, state_id, &detail,
                    );
                    let room = rows.max(1);
                    let start = (*detail_scroll).min(body.len().saturating_sub(room));
                    return body.into_iter().skip(start).take(room).collect();
                }
                if m.graph_on(width) {
                    let mut out = crate::factory_view::machine_header_lines(
                        theme,
                        width,
                        &d.target,
                        &d.description,
                        &d.params,
                    );
                    out.push(Vec::new());
                    let selected_state = d.states.get(d.selected).map(|s| s.id.as_str());
                    match d.graph.as_ref().and_then(|spec| {
                        crate::factory_view::machine_block_diagram(
                            theme,
                            spec,
                            &crate::factory_view::DiagramRenderOptions {
                                selected_state,
                                live_nodes: None,
                                max_width: Some(width.saturating_sub(2)),
                                inherit_model: Some(host_model),
                            },
                        )
                    }) {
                        Some(diagram) => {
                            let room = rows.saturating_sub(out.len()).max(1);
                            let mut start =
                                d.graph_scroll.min(diagram.lines.len().saturating_sub(room));
                            if !d.graph_manual_scroll {
                                if let Some(sel) = selected_state {
                                    if let Some((_, top, bottom)) =
                                        diagram.node_spans.iter().find(|(id, _, _)| id == sel)
                                    {
                                        start = crate::factory_view::scroll_to_span(
                                            start,
                                            *top,
                                            *bottom,
                                            room,
                                            diagram.lines.len(),
                                        );
                                    }
                                }
                            }
                            out.extend(diagram.lines.into_iter().skip(start).take(room).map(
                                |line| {
                                    let mut row = vec![Span::raw(" ")];
                                    row.extend(line);
                                    truncate_line(&row, width, "…")
                                },
                            ));
                        }
                        None => out.push(vec![theme.fg_span(
                            ThemeColor::Warning,
                            format!(
                                " No graph: {}",
                                d.graph_error
                                    .as_deref()
                                    .unwrap_or("the draft does not compile")
                            ),
                        )]),
                    }
                    return out;
                }
                let mut out: Vec<Line> = vec![vec![
                    Span::raw(" "),
                    theme.bold(theme.fg_span(ThemeColor::Text, d.target.clone())),
                    dim(if d.project.is_some() {
                        "  draft · saving needs approval again".into()
                    } else if d.target == d.name {
                        "  draft".into()
                    } else {
                        format!("  copy of {}", d.name)
                    }),
                ]];
                if let Some(roles) = d.roles.as_object().filter(|r| !r.is_empty()) {
                    let summary = roles
                        .iter()
                        .map(|(role_name, cfg)| {
                            let model =
                                cfg.get("model").and_then(Value::as_str).unwrap_or_default();
                            let thinking = cfg
                                .get("thinking")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            if thinking.is_empty() {
                                format!("{role_name}={model}")
                            } else {
                                format!("{role_name}={model} ({thinking})")
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" · ");
                    out.push(vec![
                        Span::raw(" "),
                        dim(format!(
                            "roles: {}",
                            truncate_to_width(&summary, width.saturating_sub(9), "…")
                        )),
                    ]);
                }
                out.push(Vec::new());
                let id_width = d
                    .states
                    .iter()
                    .map(|s| str_width(&s.id))
                    .max()
                    .unwrap_or(5)
                    .max(5)
                    .min(width / 2);
                let room = rows.saturating_sub(out.len());
                for i in Self::window(d.selected, d.states.len(), room) {
                    let state = &d.states[i];
                    let pad = id_width.saturating_sub(str_width(&state.id));
                    out.push(menu_row(
                        theme,
                        width,
                        vec![
                            theme.fg_span(ThemeColor::Text, state.id.clone()),
                            Span::raw(" ".repeat(pad + 2)),
                            theme.fg_span(
                                if state.editable {
                                    ThemeColor::MdLink
                                } else {
                                    ThemeColor::Muted
                                },
                                model_badge(&state.model, &state.thinking),
                            ),
                        ],
                        &[],
                        i == d.selected,
                    ));
                }
                out
            }
        }
    }

    fn render_review(r: &Review, theme: &Theme, width: usize, rows: usize) -> Vec<Line> {
        if !r.findings_focus {
            return Self::render_review_body(r, theme, width, rows);
        }
        let pane = Self::render_findings(r, theme, width, (rows / 2).max(3));
        let mut out = Self::render_review_body(r, theme, width, rows.saturating_sub(pane.len()));
        out.truncate(rows.saturating_sub(pane.len()));
        out.extend(pane);
        out
    }

    fn render_findings(r: &Review, theme: &Theme, width: usize, rows: usize) -> Vec<Line> {
        let visible = r.visible_findings();
        let title = format!(
            " Findings ({} open{})",
            r.open_findings(),
            if r.findings_all { ", all shown" } else { "" }
        );
        let mut out = vec![
            rule_row(theme, width),
            vec![theme.bold(theme.fg_span(ThemeColor::Accent, title))],
        ];
        if visible.is_empty() {
            out.push(vec![
                theme.fg_span(ThemeColor::Dim, "   No open findings · a show all")
            ]);
            return out;
        }
        let room = rows.saturating_sub(out.len()).max(1);
        for pos in Self::window(r.finding_selected, visible.len(), room) {
            let f = &r.findings[visible[pos]];
            let selected = pos == r.finding_selected;
            let (severity, color) = f.severity_badge();
            let mut line = vec![
                Span::raw(" "),
                theme.bold(theme.fg_span(ThemeColor::Accent, if selected { "›" } else { " " })),
                Span::raw(" "),
                theme.fg_span(color, format!("{severity:<4} ")),
                theme.fg_span(ThemeColor::Dim, format!("[{}] ", f.status)),
            ];
            if !f.category.is_empty() {
                line.push(theme.fg_span(ThemeColor::Muted, format!("{} ", f.category)));
            }
            if !f.file.is_empty() {
                line.push(theme.fg_span(ThemeColor::Accent, format!("{} ", f.location())));
            }
            let message = theme.fg_span(ThemeColor::Text, f.message.clone());
            line.push(if selected {
                theme.bold(message)
            } else {
                message
            });
            if f.confidence > 0.0 {
                line.push(theme.fg_span(
                    ThemeColor::Dim,
                    format!(" ({:.0}%)", f.confidence.min(1.0) * 100.0),
                ));
            }
            out.push(truncate_line(&line, width, "…"));
        }
        out
    }

    fn render_review_body(r: &Review, theme: &Theme, width: usize, rows: usize) -> Vec<Line> {
        let wide = width >= 90;
        let list_width = if wide {
            (width / 4).clamp(14, 32)
        } else {
            width
        };
        let mut files: Vec<Line> = vec![vec![theme.fg_span(
            ThemeColor::Dim,
            if r.staged { " staged" } else { " unstaged" },
        )]];
        let list_room = if wide {
            rows.saturating_sub(1)
        } else {
            rows.saturating_sub(1).min(5)
        };
        if r.files.is_empty() {
            files.push(vec![theme.fg_span(
                ThemeColor::Dim,
                if r.staged {
                    "   No staged changes"
                } else {
                    "   No unstaged changes"
                },
            )]);
        }
        for i in Self::window(r.selected, r.files.len(), list_room) {
            let (path, status) = &r.files[i];
            let selected = i == r.selected;
            let name = theme.fg_span(ThemeColor::Text, clean(path));
            files.push(truncate_line(
                &vec![
                    Span::raw(" "),
                    theme.bold(theme.fg_span(ThemeColor::Accent, if selected { "›" } else { " " })),
                    Span::raw(" "),
                    theme.fg_span(ThemeColor::Warning, format!("{status} ")),
                    if selected { theme.bold(name) } else { name },
                ],
                list_width,
                "…",
            ));
        }
        let diff_width = if wide {
            width.saturating_sub(list_width + 3)
        } else {
            width
        };
        let diff_rows = if wide {
            rows
        } else {
            rows.saturating_sub(files.len() + 1)
        };
        // A narrow pane shows one column: split halves would be too thin to read.
        let unified = r.unified || diff_width < 80;
        let diff = Self::render_diff(r, theme, diff_width, diff_rows, unified);
        if !wide {
            let mut out = files;
            out.push(rule_row(theme, width));
            out.extend(diff);
            return out;
        }
        let border = theme.fg_span(ThemeColor::BorderMuted, "│");
        (0..rows)
            .map(|row| {
                let mut line =
                    truncate_line(files.get(row).unwrap_or(&Vec::new()), list_width, "…");
                line.push(Span::raw(
                    " ".repeat(list_width.saturating_sub(line_width(&line))),
                ));
                line.push(border.clone());
                line.push(Span::raw(" "));
                line.extend(diff.get(row).cloned().unwrap_or_default());
                line
            })
            .collect()
    }

    fn side_text(side: Option<&DiffSide>, column: usize, width: usize) -> (String, &'static str) {
        let Some(side) = side else {
            return (String::new(), "");
        };
        let text: String = side.text.chars().skip(column).collect();
        let body = format!("{:>4} {} {text}", side.number, side.sign);
        (
            truncate_to_width(&body, width, "…"),
            match side.sign.as_str() {
                "-" => "-",
                "+" => "+",
                _ => " ",
            },
        )
    }

    fn sign_color(sign: &str) -> ThemeColor {
        match sign {
            "-" => ThemeColor::ToolDiffRemoved,
            "+" => ThemeColor::ToolDiffAdded,
            _ => ThemeColor::Text,
        }
    }

    fn render_diff(
        r: &Review,
        theme: &Theme,
        width: usize,
        rows: usize,
        unified: bool,
    ) -> Vec<Line> {
        if r.files.is_empty() {
            return Vec::new();
        }
        let half = width.saturating_sub(3) / 2;
        r.rows
            .iter()
            .skip(r.top)
            .take(rows)
            .map(|row| match row {
                DiffRow::Header(text) => {
                    vec![theme.fg_span(ThemeColor::Dim, truncate_to_width(text, width, "…"))]
                }
                DiffRow::Hunk(text) => {
                    vec![theme.fg_span(ThemeColor::Accent, truncate_to_width(text, width, "…"))]
                }
                DiffRow::Gap => vec![theme.fg_span(ThemeColor::Dim, "…")],
                DiffRow::Line { kind, old, new } if unified => {
                    let side = if kind == "removed" {
                        old.as_ref()
                    } else {
                        new.as_ref().or(old.as_ref())
                    };
                    let (text, sign) = Self::side_text(side, r.column, width);
                    vec![theme.fg_span(Self::sign_color(sign), text)]
                }
                DiffRow::Line { old, new, .. } => {
                    let (left, left_sign) = Self::side_text(old.as_ref(), r.column, half);
                    let (right, right_sign) = Self::side_text(new.as_ref(), r.column, half);
                    let pad = half.saturating_sub(str_width(&left));
                    vec![
                        theme.fg_span(Self::sign_color(left_sign), left),
                        Span::raw(" ".repeat(pad)),
                        theme.fg_span(ThemeColor::BorderMuted, " │ "),
                        theme.fg_span(Self::sign_color(right_sign), right),
                    ]
                }
            })
            .collect()
    }
}

fn diff_side(value: &Value) -> Option<DiffSide> {
    value.is_object().then(|| DiffSide {
        number: value["n"].as_i64().unwrap_or(0),
        sign: value["sign"].as_str().unwrap_or(" ").to_owned(),
        text: clean(value["text"].as_str().unwrap_or("")),
    })
}

fn diff_row(value: &Value) -> DiffRow {
    let text = || clean(value["text"].as_str().unwrap_or(""));
    match value["kind"].as_str() {
        Some("file") => DiffRow::Header(text()),
        Some("hunk") => DiffRow::Hunk(text()),
        Some("gap") => DiffRow::Gap,
        kind => DiffRow::Line {
            kind: kind.unwrap_or("context").to_owned(),
            old: diff_side(&value["old"]),
            new: diff_side(&value["new"]),
        },
    }
}

#[cfg(test)]
mod review_findings_tests {
    use super::*;
    use crate::theme::ColorMode;

    fn kb() -> KeybindingsManager {
        KeybindingsManager::new()
    }

    fn screen(page: &mut WorkspaceView, width: usize) -> String {
        page.set_viewport_rows(24);
        let theme = Theme::builtin("prime", ColorMode::TrueColor);
        let lines = page.render(&theme, width, &kb());
        for line in &lines {
            assert!(line_width(line) <= width);
        }
        lines
            .iter()
            .map(|line| line.iter().map(|s| s.content.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn crew() -> WorkspaceView {
        let (mut page, _) = WorkspaceView::review(Some(("/p".into(), "ada".into())));
        let WorkspaceAction::Call { args, input, .. } = page.findings_call() else {
            panic!("crew review loads findings");
        };
        assert_eq!(args, ["captain", "act", "review-findings"]);
        assert_eq!(input, json!({"project": "/p", "member": "ada"}));
        page.apply(
            WorkspaceRequest::Files,
            Ok(json!({"root": "/w", "staged": false,
                      "files": [{"path": "a.rs", "status": "M"}, {"path": "b.rs", "status": "M"}]})),
        );
        page.apply(WorkspaceRequest::Diff, Ok(json!({"rows": []})));
        page.apply(
            WorkspaceRequest::Findings,
            Ok(json!({"findings": [
                {"id": "f1", "file": "a.rs", "snippet_hash": "h1", "category": "bug",
                 "severity": "high", "confidence": 0.9, "line_hint": 12, "message": "Off by one"},
                {"id": "f2", "file": "b.rs", "snippet_hash": "h2", "category": "style",
                 "severity": "low", "message": "Rename x"},
                {"id": "f3", "file": "b.rs", "snippet_hash": "h2", "message": "duplicate"}
            ]})),
        );
        page
    }

    fn call_of(action: &WorkspaceAction) -> (WorkspaceRequest, Vec<String>, Value) {
        match action {
            WorkspaceAction::Call {
                request,
                args,
                input,
            } => (*request, args.clone(), input.clone()),
            other => panic!("expected a call, got {other:?}"),
        }
    }

    #[test]
    fn findings_badge_and_toggle_render() {
        let mut page = crew();
        let shown = screen(&mut page, 110);
        assert!(shown.contains("Findings (2 open)"), "{shown}");
        assert!(!shown.contains("Off by one"), "{shown}");
        page.handle_key("f", &kb());
        let shown = screen(&mut page, 110);
        assert!(
            shown.contains("High [open] bug a.rs:12 Off by one (90%)"),
            "{shown}"
        );
        assert!(shown.contains("Low  [open] style b.rs Rename x"), "{shown}");
        assert!(!shown.contains("duplicate"), "{shown}");
        page.handle_key("f", &kb());
        assert!(!screen(&mut page, 110).contains("Off by one"));
    }

    #[test]
    fn unsupported_findings_backend_is_silent() {
        let (mut page, _) = WorkspaceView::review(Some(("/p".into(), "ada".into())));
        page.apply(WorkspaceRequest::Findings, Err("unknown act".into()));
        let shown = screen(&mut page, 110);
        assert!(
            !shown.contains("unknown act") && !shown.contains("Findings"),
            "{shown}"
        );
        assert_eq!(page.handle_key("f", &kb()), WorkspaceAction::None);
        assert_eq!(
            WorkspaceView::review(None).0.findings_call(),
            WorkspaceAction::None
        );
    }

    #[test]
    fn triage_keys_update_and_save_and_filter() {
        let mut page = crew();
        page.handle_key("f", &kb());
        let (_, args, _) = call_of(&page.handle_key("down", &kb()));
        assert_eq!(args, ["captain", "act", "review"]);
        let (request, args, input) = call_of(&page.handle_key("r", &kb()));
        assert_eq!(request, WorkspaceRequest::Triage);
        assert_eq!(args, ["captain", "act", "review-triage"]);
        assert_eq!(
            input,
            json!({"project": "/p", "id": "f2", "status": "resolved"})
        );
        let shown = screen(&mut page, 110);
        assert!(
            shown.contains("Findings (1 open)") && !shown.contains("Rename x"),
            "{shown}"
        );
        call_of(&page.handle_key("i", &kb()));
        assert!(screen(&mut page, 110).contains("Findings (0 open)"));
        page.handle_key("a", &kb());
        let shown = screen(&mut page, 110);
        assert!(shown.contains("[resolved] style b.rs Rename x"), "{shown}");
        assert!(shown.contains("[irrelevant] bug a.rs:12"), "{shown}");
        let (_, _, input) = call_of(&page.handle_key("o", &kb()));
        assert_eq!(input["status"], "open");
        assert_eq!(
            page.apply(WorkspaceRequest::Triage, Ok(json!({}))),
            WorkspaceAction::None
        );
    }

    #[test]
    fn rollback_confirms_before_calling() {
        let mut page = crew();
        assert_eq!(page.handle_key("R", &kb()), WorkspaceAction::None);
        assert!(screen(&mut page, 110).contains(ROLLBACK_PROMPT));
        assert_eq!(page.handle_key("n", &kb()), WorkspaceAction::None);
        page.handle_key("R", &kb());
        assert_eq!(page.handle_key("escape", &kb()), WorkspaceAction::None);
        page.handle_key("R", &kb());
        let (request, args, input) = call_of(&page.handle_key("y", &kb()));
        assert_eq!(request, WorkspaceRequest::Rollback);
        assert_eq!(args, ["captain", "act", "checkpoint-rollback"]);
        assert_eq!(input, json!({"project": "/p", "member": "ada"}));
        let (request, _, _) = call_of(&page.apply(WorkspaceRequest::Rollback, Ok(json!({}))));
        assert_eq!(request, WorkspaceRequest::Files);
        assert!(screen(&mut page, 110).contains("Rolled back to the last pre-turn checkpoint."));
        let (mut local, _) = WorkspaceView::review(None);
        local.handle_key("R", &kb());
        assert!(!screen(&mut local, 110).contains(ROLLBACK_PROMPT));
    }
}
