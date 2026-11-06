//! Reusable interactive browser and CLI-specific data adapters.

use anyhow::{Context, Result};
use arboard::Clipboard;
use ratatui::{
    Frame,
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use serde_json::{Map, Value, json};
use sift_core::{ProgramInfo, Registry, nodes, summary::type_label};
use sift_types::{IdlSource, ProgramSchema};
use std::{
    collections::BTreeMap,
    io::IsTerminal,
    time::{Duration, Instant},
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

pub(crate) enum AccountPageUpdate {
    Discovered {
        addresses: Vec<String>,
        account_type: String,
    },
    Loaded {
        document: BrowserDocument,
        start: usize,
        scanned: usize,
        total: usize,
    },
    Failed(String),
}

pub(crate) struct AccountPaging {
    updates: UnboundedReceiver<AccountPageUpdate>,
    requests: UnboundedSender<()>,
    loading: bool,
    has_more: bool,
    message: String,
    loaded: usize,
    scanned: usize,
    retry_required: bool,
    request_after_draw: bool,
}

impl AccountPaging {
    pub(crate) fn new(
        updates: UnboundedReceiver<AccountPageUpdate>,
        requests: UnboundedSender<()>,
    ) -> Self {
        Self {
            updates,
            requests,
            loading: true,
            has_more: true,
            message: "Finding matching accounts…".into(),
            loaded: 0,
            scanned: 0,
            retry_required: false,
            request_after_draw: false,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DecodedAccount {
    pub(crate) address: String,
    pub(crate) account_type: String,
    pub(crate) data: Value,
    pub(crate) seeds: Option<String>,
    /// A non-fatal limitation encountered while decoding this account.
    pub(crate) note: Option<String>,
}
#[derive(Clone, Debug)]
pub(crate) struct RequestMetadata {
    pub(crate) account_type: String,
    pub(crate) program: String,
    pub(crate) idl_location: String,
    pub(crate) rpc_url: String,
    pub(crate) commitment: String,
}
#[derive(Clone, Debug)]
pub(crate) struct BrowserDocument {
    title: String,
    header: Header,
    raw: Value,
    nodes: Vec<Node>,
}
#[derive(Clone, Debug)]
struct Header {
    title: String,
    rows: Vec<Row>,
}
#[derive(Clone, Debug)]
struct Row {
    left: Field,
    right: Option<Field>,
}
#[derive(Clone, Debug)]
struct Field {
    label: String,
    value: String,
}
#[derive(Clone, Debug)]
struct Node {
    label: String,
    ty: String,
    preview: String,
    raw: Value,
    children: Vec<Node>,
    bytes: Option<Vec<u8>>,
    openable: bool,
    warning: bool,
}

pub(crate) fn should_run(no_tui: bool, has_items: bool) -> bool {
    !no_tui && has_items && std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}
pub(crate) fn account_list_json(accounts: &[DecodedAccount]) -> Value {
    Value::Array(accounts.iter().map(DecodedAccount::as_json).collect())
}
impl DecodedAccount {
    fn as_json(&self) -> Value {
        let mut result = Map::new();
        result.insert("address".into(), Value::String(self.address.clone()));
        result.insert("type".into(), Value::String(self.account_type.clone()));
        result.insert("data".into(), self.data.clone());
        if let Some(seeds) = &self.seeds {
            result.insert("seeds".into(), Value::String(seeds.clone()));
        }
        if let Some(note) = &self.note {
            result.insert("note".into(), Value::String(note.clone()));
        }
        Value::Object(result)
    }
}

pub(crate) fn account_document(
    accounts: &[DecodedAccount],
    ty: &Value,
    definitions: &BTreeMap<String, Value>,
    metadata: RequestMetadata,
) -> BrowserDocument {
    BrowserDocument {
        title: "Decoded accounts".into(),
        raw: account_list_json(accounts),
        header: Header {
            title: "Request".into(),
            rows: vec![
                row(
                    "Account type",
                    metadata.account_type,
                    Some(("Program", metadata.program)),
                ),
                row(
                    "RPC",
                    metadata.rpc_url,
                    Some(("Commitment", metadata.commitment)),
                ),
                row("IDL", metadata.idl_location, None),
            ],
        },
        nodes: accounts
            .iter()
            .map(|account| {
                let mut children = account_children(&account.data, ty, definitions);
                if let Some(note) = &account.note {
                    children.insert(0, warning_node("Note", note));
                }
                Node {
                    label: account.address.clone(),
                    ty: account.account_type.clone(),
                    preview: account.seeds.clone().unwrap_or_default(),
                    raw: account.data.clone(),
                    children,
                    bytes: None,
                    openable: true,
                    warning: false,
                }
            })
            .collect(),
    }
}

pub(crate) fn idl_document(registry: &Registry) -> BrowserDocument {
    let source_count = registry
        .config
        .programs
        .values()
        .map(|program| program.sources.len())
        .sum::<usize>();
    let nodes = registry
        .config
        .programs
        .iter()
        .map(|(address, program)| {
            let aliases = program.aliases.iter().cloned().collect::<Vec<_>>();
            let sources = program
                .priority
                .iter()
                .filter_map(|id| program.sources.get(id).map(|source| source_node(id, source)))
                .collect::<Vec<_>>();
            Node {
                label: address.clone(),
                ty: "program".into(),
                preview: format!(
                    "{}, {}",
                    count_label(aliases.len(), "alias"),
                    count_label(sources.len(), "source")
                ),
                raw: json!({"address":address,"aliases":aliases,"sources":program.sources,"priority":program.priority}),
                children: vec![
                    Node {
                        label: "Aliases".into(),
                        ty: String::new(),
                        preview: aliases
                            .iter()
                            .map(|alias| alias.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        raw: json!(aliases),
                        children: aliases.iter().map(|alias| alias_node(alias)).collect(),
                        bytes: None,
                        openable: true,
                        warning: false,
                    },
                    Node {
                        label: "Sources".into(),
                        ty: String::new(),
                        preview: count_label(sources.len(), "source"),
                        raw: serde_json::to_value(&program.sources).expect("sources serialize"),
                        children: sources,
                        bytes: None,
                        openable: true,
                        warning: false,
                    },
                    Node {
                        label: "Priority".into(),
                        ty: String::new(),
                        preview: program.priority.join(", "),
                        raw: json!(program.priority),
                        children: program.priority.iter().map(|id| source_field(id, json!(id), id.clone())).collect(),
                        bytes: None,
                        openable: true,
                        warning: false,
                    },
                ],
                bytes: None,
                openable: true,
                warning: false,
            }
        })
        .collect();
    BrowserDocument {
        title: "IDL registry".into(),
        raw: registry.json(),
        nodes,
        header: Header {
            title: "Registry".into(),
            rows: vec![
                row(
                    "Path",
                    registry.path.display().to_string(),
                    Some(("Programs", registry.config.programs.len().to_string())),
                ),
                row(
                    "Sources",
                    source_count.to_string(),
                    Some(("Schema version", registry.config.schema_version.to_string())),
                ),
            ],
        },
    }
}
/// One registered program and the outcome of inspecting it on chain.
pub(crate) struct ListedProgram {
    pub(crate) address: String,
    pub(crate) aliases: Vec<String>,
    pub(crate) info: Result<ProgramInfo, String>,
}

impl ListedProgram {
    pub(crate) fn as_json(&self) -> Value {
        match &self.info {
            Ok(info) => {
                let mut value = serde_json::to_value(info).expect("program info serializes");
                value["aliases"] = json!(self.aliases);
                value
            }
            Err(error) => json!({"address": self.address, "aliases": self.aliases, "error": error}),
        }
    }
}

pub(crate) fn program_list_document(programs: &[ListedProgram], rpc_url: &str) -> BrowserDocument {
    let raw = Value::Array(programs.iter().map(ListedProgram::as_json).collect());
    let nodes = programs
        .iter()
        .map(|program| {
            let raw = program.as_json();
            let mut node = source_field(&program.address, raw.clone(), String::new());
            let mut children = vec![source_field(
                "Aliases",
                json!(program.aliases),
                program.aliases.join(", "),
            )];
            match &program.info {
                Ok(info) => {
                    node.preview = program_flags(info, &program.aliases);
                    children.extend(program_info_children(info, &raw));
                }
                Err(error) => {
                    node.preview = error.clone();
                    node.warning = true;
                    children.push(source_field("Error", json!(error), error.clone()));
                }
            }
            node.children = children;
            node
        })
        .collect();
    let verified = programs
        .iter()
        .filter(|program| matches!(&program.info, Ok(info) if info.verified_build.is_some()))
        .count();
    BrowserDocument {
        title: "Programs".into(),
        header: Header {
            title: "Registered programs".into(),
            rows: vec![
                row(
                    "Programs",
                    programs.len().to_string(),
                    Some(("Verified", verified.to_string())),
                ),
                row("RPC", rpc_url.to_owned(), None),
            ],
        },
        raw,
        nodes,
    }
}

fn program_flags(info: &ProgramInfo, aliases: &[String]) -> String {
    let mut flags = Vec::new();
    if !aliases.is_empty() {
        flags.push(aliases.join(", "));
    }
    flags.push(
        if info.authority.is_some() {
            "upgradeable"
        } else {
            "immutable"
        }
        .to_owned(),
    );
    if info.idl_address.is_some() {
        flags.push("on-chain IDL".to_owned());
    }
    if info.verified_build.is_some() {
        flags.push("verified".to_owned());
    }
    flags.join(" · ")
}

fn program_info_children(info: &ProgramInfo, raw: &Value) -> Vec<Node> {
    let optional = |label: &str, value: &Option<String>, missing: &str| {
        source_field(
            label,
            json!(value),
            value.clone().unwrap_or_else(|| missing.to_owned()),
        )
    };
    let mut children = vec![
        source_field("Loader", json!(info.loader), info.loader.clone()),
        optional("ProgramData", &info.programdata_address, "none"),
        optional("Authority", &info.authority, "none (immutable)"),
    ];
    if let Some(slot) = info.last_deployed_slot {
        children.push(source_field("Deployed slot", json!(slot), slot.to_string()));
    }
    children.push(optional("IDL", &info.idl_address, "none"));
    let mut verified = source_field(
        "Verified build",
        raw["verified_build"].clone(),
        info.verified_build
            .as_ref()
            .map(|build| build.repo_url.clone())
            .unwrap_or_else(|| "none".to_owned()),
    );
    if let Some(build) = &info.verified_build {
        verified.children = vec![
            source_field("Repository", json!(build.repo_url), build.repo_url.clone()),
            source_field("Commit", json!(build.commit), build.commit.clone()),
            source_field(
                "Hash",
                json!(build.executable_hash),
                build.executable_hash.clone(),
            ),
        ];
        if let Some(at) = &build.last_verified_at {
            verified
                .children
                .push(source_field("Verified at", json!(at), at.clone()));
        }
    }
    children.push(verified);
    children
}

pub(crate) fn idl_schema_document(schema: &ProgramSchema, idl_location: String) -> BrowserDocument {
    let raw = serde_json::to_value(schema).expect("program schema serializes");
    let nodes = [
        ("Accounts", "accounts", "account"),
        ("Instructions", "instructions", "instruction"),
        ("Types", "types", "type"),
    ]
    .into_iter()
    .map(|(label, key, noun)| {
        let value = &raw[key];
        let mut node = generic_node(label, value);
        if let Some(items) = value.as_array() {
            node.children = items
                .iter()
                .map(|item| generic_node(item["name"].as_str().unwrap_or("unnamed"), item))
                .collect();
        }
        node.preview = count_label(node.children.len(), noun);
        node
    })
    .collect();
    BrowserDocument {
        title: "IDL schema".into(),
        header: Header {
            title: "Program".into(),
            rows: vec![
                row(
                    "Name",
                    schema.name.clone(),
                    Some(("Address", schema.address.clone())),
                ),
                row(
                    "IDL",
                    idl_location,
                    Some(("Format", schema.format.label().to_owned())),
                ),
            ],
        },
        raw,
        nodes,
    }
}

fn row(label: &str, value: String, right: Option<(&str, String)>) -> Row {
    Row {
        left: Field {
            label: label.into(),
            value,
        },
        right: right.map(|(label, value)| Field {
            label: label.into(),
            value,
        }),
    }
}
fn alias_node(alias: &str) -> Node {
    Node {
        label: alias.into(),
        ty: String::new(),
        preview: String::new(),
        raw: json!(alias),
        children: vec![],
        bytes: None,
        openable: true,
        warning: false,
    }
}

fn source_field(label: &str, raw: Value, preview: String) -> Node {
    Node {
        label: label.into(),
        ty: String::new(),
        preview,
        raw,
        children: vec![],
        bytes: None,
        openable: true,
        warning: false,
    }
}
fn source_node(id: &str, source: &IdlSource) -> Node {
    let (ty, location) = match source {
        IdlSource::File { path, .. } => ("file", path.display().to_string()),
        IdlSource::Url { url, .. } => ("url", url.to_string()),
        IdlSource::OnChain { address, .. } => ("on-chain", address.clone()),
    };
    let raw = serde_json::to_value(source).expect("source serializes");
    Node {
        label: format!("[{id}] {ty}"),
        ty: String::new(),
        preview: location,
        raw: raw.clone(),
        children: source_children(source),
        bytes: None,
        openable: true,
        warning: false,
    }
}

fn source_children(source: &IdlSource) -> Vec<Node> {
    match source {
        IdlSource::File { name, path } => {
            let name = display_file_source_name(name, path);
            vec![
                source_field("Name", json!(name), name),
                source_field("Path", json!(path), path.display().to_string()),
            ]
        }
        IdlSource::Url { name, url } => {
            vec![
                source_field("Name", json!(name), name.clone()),
                source_field("URL", json!(url), url.to_string()),
            ]
        }
        IdlSource::OnChain { name, address } => vec![
            source_field("Name", json!(name), name.clone()),
            source_field("Address", json!(address), address.clone()),
        ],
    }
}

fn display_file_source_name(name: &str, path: &std::path::Path) -> String {
    if name == path.display().to_string() {
        path.file_name()
            .map(|file_name| file_name.to_string_lossy().into_owned())
            .unwrap_or_else(|| name.to_owned())
    } else {
        name.to_owned()
    }
}

fn count_label(count: usize, noun: &str) -> String {
    let plural = if count == 1 {
        noun.to_owned()
    } else if let Some(stem) = noun.strip_suffix('s') {
        format!("{stem}ses")
    } else if let Some(stem) = noun.strip_suffix('y') {
        format!("{stem}ies")
    } else {
        format!("{noun}s")
    };
    format!("{count} {plural}")
}

pub(crate) fn run(document: BrowserDocument) -> Result<()> {
    run_browser(document, None)
}

pub(crate) fn run_paged_accounts(document: BrowserDocument, paging: AccountPaging) -> Result<()> {
    run_browser(document, Some(paging))
}

fn run_browser(document: BrowserDocument, paging: Option<AccountPaging>) -> Result<()> {
    let mut terminal = ratatui::try_init().context("cannot initialize the interactive browser")?;
    let mut app = App::new(document);
    app.paging = paging;
    let result = app.run(&mut terminal);
    let restore =
        ratatui::try_restore().context("cannot restore the terminal after the interactive browser");
    restore?;
    result
}
#[derive(Clone, Debug)]
enum Presentation {
    Tree,
    Bytes(Vec<u8>),
    Json,
}
#[derive(Clone, Debug)]
struct View {
    title: String,
    raw: Value,
    nodes: Vec<Node>,
    presentation: Presentation,
}

fn view_json(view: &View, paged_accounts: bool) -> Value {
    if paged_accounts {
        let accounts = view.raw.as_array().expect("account list is an array");
        Value::Array(
            view.nodes
                .iter()
                .zip(accounts)
                .map(|(node, raw)| {
                    if raw.is_null() {
                        json!({"address": node.label, "type": node.ty, "status": "pending"})
                    } else {
                        raw.clone()
                    }
                })
                .collect(),
        )
    } else {
        view.raw.clone()
    }
}
#[derive(Debug)]
struct Raw {
    text: String,
    scroll: u16,
}
struct App {
    header: Header,
    view: View,
    history: Vec<(View, usize)>,
    selected: usize,
    raw: Option<Raw>,
    status: Option<(String, Instant)>,
    paging: Option<AccountPaging>,
}
impl App {
    fn new(document: BrowserDocument) -> Self {
        Self {
            header: document.header,
            view: View {
                title: document.title,
                raw: document.raw,
                nodes: document.nodes,
                presentation: Presentation::Tree,
            },
            history: vec![],
            selected: 0,
            raw: None,
            status: None,
            paging: None,
        }
    }
    fn run(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        loop {
            if self.poll_account_pages() {
                // RPC diagnostics can write to stderr while a request is pending.
                terminal.clear()?;
            }
            if self
                .status
                .as_ref()
                .is_some_and(|(_, time)| time.elapsed() > Duration::from_secs(3))
            {
                self.status = None;
            }
            terminal.draw(|frame| self.render(frame))?;
            self.request_account_page();
            if event::poll(Duration::from_millis(250))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
                && self.key(key)
            {
                return Ok(());
            }
        }
    }
    fn poll_account_pages(&mut self) -> bool {
        let Some(paging) = &mut self.paging else {
            return false;
        };
        let mut redraw = false;
        while let Ok(update) = paging.updates.try_recv() {
            let root = self
                .history
                .first_mut()
                .map(|(view, _)| view)
                .unwrap_or(&mut self.view);
            match update {
                AccountPageUpdate::Discovered {
                    addresses,
                    account_type,
                } => {
                    // Pending JSON is materialized only when requested, even for large address lists.
                    root.raw = Value::Array(vec![Value::Null; addresses.len()]);
                    root.nodes = addresses
                        .into_iter()
                        .map(|address| Node {
                            label: address,
                            ty: account_type.clone(),
                            preview: String::new(),
                            raw: Value::Null,
                            children: vec![],
                            bytes: None,
                            openable: false,
                            warning: false,
                        })
                        .collect();
                    paging.request_after_draw = true;
                    paging.message = format!(
                        "{} addresses found · Loading account details…",
                        root.nodes.len()
                    );
                }
                AccountPageUpdate::Loaded {
                    document,
                    start,
                    scanned,
                    total,
                } => {
                    paging.loaded += document.nodes.len();
                    let mut decoded = document
                        .nodes
                        .into_iter()
                        .zip(
                            document
                                .raw
                                .as_array()
                                .expect("account list is an array")
                                .iter()
                                .cloned(),
                        )
                        .map(|(node, raw)| (node.label.clone(), (node, raw)))
                        .collect::<BTreeMap<_, _>>();
                    let raw = root.raw.as_array_mut().expect("account list is an array");
                    for (node, raw) in root.nodes[start..scanned]
                        .iter_mut()
                        .zip(&mut raw[start..scanned])
                    {
                        if let Some((loaded_node, loaded_raw)) = decoded.remove(&node.label) {
                            *node = loaded_node;
                            *raw = loaded_raw;
                        } else {
                            *raw = json!({"address": node.label, "type": node.ty, "status": "unavailable"});
                            node.raw = raw.clone();
                            node.preview = "Account unavailable or could not decode".into();
                            node.warning = true;
                            node.openable = true;
                        }
                    }
                    paging.has_more = scanned < total;
                    paging.scanned = scanned;
                    paging.loading = false;
                    paging.retry_required = false;
                    paging.request_after_draw = false;
                    paging.message = format!(
                        "{} loaded · {scanned}/{total} checked · {}",
                        paging.loaded,
                        if paging.has_more {
                            "n next 100 · scroll near the end to load more"
                        } else {
                            "complete"
                        }
                    );
                }
                AccountPageUpdate::Failed(error) => {
                    redraw = true;
                    paging.loading = false;
                    paging.retry_required = true;
                    paging.message = format!("Could not load accounts: {error} · n retry");
                    paging.has_more = true;
                }
            }
            if self.history.is_empty()
                && let Some(raw) = &mut self.raw
            {
                raw.text = raw_text(&view_json(&self.view, true));
            }
        }
        redraw
    }

    fn request_account_page(&mut self) {
        if let Some(paging) = &mut self.paging
            && std::mem::take(&mut paging.request_after_draw)
        {
            let _ = paging.requests.send(());
        }
    }

    fn load_next_account_page(&mut self) {
        if let Some(paging) = &mut self.paging
            && !paging.loading
            && paging.has_more
            && paging.requests.send(()).is_ok()
        {
            paging.loading = true;
            paging.retry_required = false;
            paging.message = "Loading next account page…".into();
        }
    }

    fn key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return true;
        }
        if key.code == KeyCode::Char('n') && self.paging.is_some() {
            self.load_next_account_page();
            return false;
        }
        if self.raw.is_some() {
            let mut copy = None;
            let raw = self.raw.as_mut().expect("checked");
            match key.code {
                KeyCode::Char('q') => return true,
                KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('r') => self.raw = None,
                KeyCode::Up => raw.scroll = raw.scroll.saturating_sub(1),
                KeyCode::Down => raw.scroll = raw.scroll.saturating_add(1),
                KeyCode::PageUp => raw.scroll = raw.scroll.saturating_sub(10),
                KeyCode::PageDown => raw.scroll = raw.scroll.saturating_add(10),
                KeyCode::Char('c') => copy = Some(raw.text.clone()),
                _ => {}
            }
            if let Some(text) = copy {
                self.copy(text);
            }
            return false;
        }
        match key.code {
            KeyCode::Char('q') => true,
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                false
            }
            KeyCode::Down => {
                if !self.view.nodes.is_empty() {
                    self.selected = (self.selected + 1).min(self.view.nodes.len() - 1);
                }
                if self.history.is_empty()
                    && self.paging.as_ref().is_some_and(|paging| {
                        paging.scanned > 0
                            && !paging.retry_required
                            && self.selected >= paging.scanned.saturating_sub(10)
                    })
                {
                    self.load_next_account_page();
                }
                false
            }
            KeyCode::Enter => {
                self.down();
                false
            }
            KeyCode::Esc if self.history.is_empty() => true,
            KeyCode::Esc | KeyCode::Backspace => {
                self.back();
                false
            }
            KeyCode::Char('r') => {
                self.raw = Some(Raw {
                    text: raw_text(&view_json(
                        &self.view,
                        self.paging.is_some() && self.history.is_empty(),
                    )),
                    scroll: 0,
                });
                false
            }
            KeyCode::Char('c') => {
                self.copy(raw_text(&view_json(
                    &self.view,
                    self.paging.is_some() && self.history.is_empty(),
                )));
                false
            }
            _ => false,
        }
    }
    fn down(&mut self) {
        let Some(node) = self.view.nodes.get(self.selected).cloned() else {
            return;
        };
        if !node.openable {
            return;
        }
        let next = View {
            title: node.label,
            raw: node.raw,
            presentation: if let Some(bytes) = node.bytes {
                Presentation::Bytes(bytes)
            } else if node.children.is_empty() {
                Presentation::Json
            } else {
                Presentation::Tree
            },
            nodes: node.children,
        };
        let previous = std::mem::replace(&mut self.view, next);
        self.history.push((previous, self.selected));
        self.selected = 0;
    }
    fn back(&mut self) {
        if let Some((view, selected)) = self.history.pop() {
            self.view = view;
            self.selected = selected;
        }
    }
    fn copy(&mut self, text: String) {
        self.status = Some((
            match Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text)) {
                Ok(()) => "Copied JSON to clipboard".into(),
                Err(error) => format!("Could not copy to clipboard: {error}"),
            },
            Instant::now(),
        ));
    }
    fn block(title: &str) -> Block<'_> {
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(title)
    }
    fn render(&self, frame: &mut Frame) {
        let header_height =
            u16::try_from(self.header.rows.len().saturating_add(2)).unwrap_or(u16::MAX);
        let areas = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(header_height),
            Constraint::Length(0),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(frame.area());
        self.header(frame, areas[1]);
        if let Some(raw) = &self.raw {
            frame.render_widget(
                Paragraph::new(raw.text.as_str())
                    .block(Self::block("Raw JSON"))
                    .scroll((raw.scroll, 0))
                    .wrap(Wrap { trim: false }),
                areas[3],
            );
            self.footer(
                frame,
                areas[4],
                "↑/↓ scroll  c copy  r/Esc back  q/Ctrl-C quit",
            );
            return;
        }
        match &self.view.presentation {
            Presentation::Tree => self.tree(frame, areas[3]),
            Presentation::Bytes(bytes) => frame.render_widget(
                Paragraph::new(format!("{} byte(s)\n\n{}", bytes.len(), hex_dump(bytes)))
                    .block(Self::block(&self.view.title))
                    .wrap(Wrap { trim: false }),
                areas[3],
            ),
            Presentation::Json => frame.render_widget(
                Paragraph::new(raw_text(&self.view.raw))
                    .block(Self::block(&self.view.title))
                    .wrap(Wrap { trim: false }),
                areas[3],
            ),
        };
        self.footer(
            frame,
            areas[4],
            "↑/↓ move  Enter open  r raw  c copy  Esc back/quit  q/Ctrl-C quit",
        );
    }
    fn header(&self, frame: &mut Frame, area: Rect) {
        let block = Self::block(&self.header.title);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let label = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD);
        let value = Style::default().fg(Color::Cyan);
        let rows =
            Layout::vertical(vec![Constraint::Length(1); self.header.rows.len()]).split(inner);
        for (row, area) in self.header.rows.iter().zip(rows.iter()) {
            let cols = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(*area);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(format!("{}: ", row.left.label), label),
                    Span::styled(&row.left.value, value),
                ])),
                cols[0],
            );
            if let Some(right) = &row.right {
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(format!("{}: ", right.label), label),
                        Span::styled(&right.value, value),
                    ]))
                    .alignment(Alignment::Right),
                    cols[1],
                );
            }
        }
    }
    fn tree(&self, frame: &mut Frame, area: Rect) {
        if self.view.nodes.is_empty()
            && let Some(paging) = &self.paging
        {
            frame.render_widget(
                Paragraph::new(paging.message.as_str()).block(Self::block(&self.view.title)),
                area,
            );
            return;
        }
        let width = usize::from(area.width.saturating_sub(36)).max(12);
        let visible_count = usize::from(area.height.saturating_sub(2)).max(1);
        let start = self.selected.saturating_sub(visible_count - 1);
        let items: Vec<_> = self
            .view
            .nodes
            .iter()
            .skip(start)
            .take(visible_count)
            .map(|node| {
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{:<18}", node.label),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::raw("  "),
                    Span::styled(truncate(&node.ty, 16), Style::default().fg(Color::Yellow)),
                    Span::raw("  "),
                    Span::styled(
                        truncate(
                            if !node.openable && node.raw.is_null() {
                                "Waiting for details…"
                            } else {
                                &node.preview
                            },
                            width,
                        ),
                        if node.warning {
                            Style::default().fg(Color::Yellow)
                        } else {
                            Style::default()
                        },
                    ),
                ]))
            })
            .collect();
        let list = List::new(items)
            .block(Self::block(&self.view.title))
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("› ");
        let mut state = ListState::default();
        state.select(Some(self.selected.saturating_sub(start)));
        frame.render_stateful_widget(list, area, &mut state);
    }
    fn footer(&self, frame: &mut Frame, area: Rect, help: &str) {
        let mut lines = vec![Line::raw(
            self.status.as_ref().map_or(help, |(message, _)| message),
        )];
        if let Some(paging) = &self.paging {
            lines.push(Line::raw(&paging.message));
        }
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().fg(Color::DarkGray)),
            area,
        )
    }
}

fn account_children(value: &Value, ty: &Value, defs: &BTreeMap<String, Value>) -> Vec<Node> {
    let shaped = nodes::shape(ty, defs);
    if let Some(fields) = nodes::struct_fields(shaped) {
        return fields
            .iter()
            .filter(|field| !nodes::field_is_omitted(field))
            .filter_map(|field| {
                let name = field.get("name").and_then(Value::as_str)?;
                let field_ty = field.get("type")?;
                Some(account_node(
                    name,
                    value.get(name).unwrap_or(&Value::Null),
                    field_ty,
                    defs,
                ))
            })
            .collect();
    }
    if let Some(values) = value.as_array()
        && let Some(element) = nodes::sequence_item(shaped)
    {
        return values
            .iter()
            .enumerate()
            .map(|(index, value)| account_node(&format!("[{index}]"), value, element, defs))
            .collect();
    }
    generic_children(value)
}
fn account_node(label: &str, value: &Value, ty: &Value, defs: &BTreeMap<String, Value>) -> Node {
    let bytes = byte_array(value, ty, defs);
    let children = if bytes.is_some() {
        vec![]
    } else {
        account_children(value, ty, defs)
    };
    Node {
        label: label.into(),
        ty: type_label(ty),
        preview: typed_preview(value, ty, defs),
        raw: value.clone(),
        children,
        bytes,
        openable: true,
        warning: false,
    }
}
fn generic_children(value: &Value) -> Vec<Node> {
    match value {
        Value::Object(values) => values
            .iter()
            .map(|(label, value)| generic_node(label, value))
            .collect(),
        Value::Array(values) => values
            .iter()
            .enumerate()
            .map(|(index, value)| generic_node(&format!("[{index}]"), value))
            .collect(),
        _ => vec![],
    }
}
fn generic_node(label: &str, value: &Value) -> Node {
    Node {
        label: label.into(),
        ty: match value {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
        .into(),
        preview: preview(value, 80),
        raw: value.clone(),
        children: generic_children(value),
        bytes: None,
        openable: true,
        warning: false,
    }
}
fn warning_node(label: &str, note: &str) -> Node {
    Node {
        label: label.into(),
        ty: "note".into(),
        preview: note.into(),
        raw: Value::String(note.into()),
        children: vec![],
        bytes: None,
        openable: true,
        warning: true,
    }
}
fn byte_array(value: &Value, ty: &Value, defs: &BTreeMap<String, Value>) -> Option<Vec<u8>> {
    nodes::is_byte_sequence(ty, defs).then_some(())?;
    value
        .as_array()?
        .iter()
        .map(|value| {
            value
                .as_u64()
                .filter(|value| *value <= 255)
                .map(|value| value as u8)
        })
        .collect()
}
fn typed_preview(value: &Value, ty: &Value, defs: &BTreeMap<String, Value>) -> String {
    if let Some(bytes) = byte_array(value, ty, defs) {
        let shown = bytes
            .iter()
            .take(8)
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        return format!(
            "{} bytes [{shown}{}]",
            bytes.len(),
            if bytes.len() > 8 { " …" } else { "" }
        );
    }
    preview(value, 80)
}
fn preview(value: &Value, width: usize) -> String {
    truncate(
        &serde_json::to_string(value).unwrap_or_else(|_| "<unrenderable>".into()),
        width,
    )
}
fn truncate(value: &str, width: usize) -> String {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(width).collect::<String>();
    if chars.next().is_some() && width > 1 {
        format!("{}…", prefix.chars().take(width - 1).collect::<String>())
    } else {
        prefix
    }
}
fn raw_text(value: &Value) -> String {
    serde_json::to_string_pretty(value)
        .unwrap_or_else(|error| format!("<cannot render JSON: {error}>"))
}
fn hex_dump(bytes: &[u8]) -> String {
    std::iter::once(format!("{:<8}  {:<47}  ASCII", "Offset", "Hex bytes"))
        .chain(bytes.chunks(16).enumerate().map(|(index, chunk)| {
            let hex = chunk
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            let text = chunk
                .iter()
                .map(|byte| {
                    if byte.is_ascii_graphic() || *byte == b' ' {
                        char::from(*byte)
                    } else {
                        '.'
                    }
                })
                .collect::<String>();
            format!("{:08x}  {:<47}  {text}", index * 16, hex)
        }))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn account() -> DecodedAccount {
        DecodedAccount {
            address: "Example111111111111111111111111111111111".into(),
            account_type: "State".into(),
            data: json!({"bytes":[65,66,0],"count":"7"}),
            seeds: None,
            note: None,
        }
    }

    fn account_type() -> Value {
        sift_core::anchor::convert_type(
            &json!({"kind":"struct","fields":[{"name":"bytes","type":{"vec":"u8"}},{"name":"count","type":"u64"}]}),
        )
        .unwrap()
    }

    fn metadata() -> RequestMetadata {
        RequestMetadata {
            account_type: "State".into(),
            program: "Program".into(),
            idl_location: "~/example.json".into(),
            rpc_url: "https://api.devnet.solana.com".into(),
            commitment: "confirmed".into(),
        }
    }

    #[test]
    fn account_adapter_preserves_byte_nodes_and_root_raw_json() {
        let document =
            account_document(&[account()], &account_type(), &BTreeMap::new(), metadata());
        assert!(document.raw.to_string().contains("Example"));
        assert_eq!(document.nodes[0].children[0].bytes, Some(vec![65, 66, 0]));
    }

    #[test]
    fn account_adapter_surfaces_non_fatal_decode_notes() {
        let mut decoded = account();
        decoded.note = Some("Parsed with 32 unmodelled trailing byte(s).".into());

        let document = account_document(&[decoded], &account_type(), &BTreeMap::new(), metadata());

        assert!(document.raw.to_string().contains("unmodelled trailing"));
        assert!(!document.nodes[0].warning);
        assert_eq!(document.nodes[0].children[0].label, "Note");
        assert!(document.nodes[0].children[0].warning);
    }

    #[test]
    fn idl_adapter_exposes_programs_aliases_and_sources() {
        let mut registry = Registry {
            path: "registry.toml".into(),
            config: Default::default(),
        };
        registry
            .add_source(
                "11111111111111111111111111111111",
                IdlSource::File {
                    name: "example".into(),
                    path: "example.json".into(),
                },
                Some("example".into()),
            )
            .unwrap();
        let document = idl_document(&registry);
        assert_eq!(document.nodes.len(), 1);
        assert_eq!(document.nodes[0].children[0].label, "Aliases");
        assert_eq!(document.nodes[0].preview, "1 alias, 1 source");
        assert!(
            document.nodes[0].children[1].children[0]
                .preview
                .contains("example.json")
        );
    }

    #[test]
    fn schema_browser_navigates_named_entries_and_preserves_json() {
        let schema = sift_core::parse_idl(
            &json!({
                "name": "example",
                "accounts": [{"name":"State", "type":{"kind":"struct","fields":[{"name":"count","type":"u64"}]}}],
                "instructions": [{"name":"ping", "accounts":[], "args":[]}],
                "types": [{"name":"Counter", "type":{"kind":"struct", "fields":[]}}]
            })
            .to_string(),
            "11111111111111111111111111111111",
        )
        .unwrap();
        let document = idl_schema_document(&schema, "example.json".into());
        assert_eq!(document.raw, serde_json::to_value(&schema).unwrap());
        assert_eq!(document.nodes[0].preview, "1 account");
        assert_eq!(document.nodes[0].children[0].label, "State");
        assert_eq!(document.nodes[1].children[0].label, "ping");
        assert_eq!(document.nodes[2].children[0].label, "Counter");

        let mut app = App::new(document);
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.view.title, "Accounts");
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.view.title, "State");
        app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert_eq!(
            serde_json::from_str::<Value>(&app.raw.as_ref().unwrap().text).unwrap(),
            serde_json::to_value(&schema.accounts[0]).unwrap()
        );
    }

    #[test]
    fn arriving_account_pages_preserve_the_open_detail_and_update_root_json() {
        let (updates_tx, updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let (requests_tx, mut requests_rx) = tokio::sync::mpsc::unbounded_channel();
        let document = |accounts: &[DecodedAccount]| {
            account_document(accounts, &account_type(), &BTreeMap::new(), metadata())
        };
        let mut app = App::new(document(&[]));
        app.paging = Some(AccountPaging::new(updates_rx, requests_tx));
        updates_tx
            .send(AccountPageUpdate::Discovered {
                addresses: vec![account().address, "NextAccount".into()],
                account_type: "State".into(),
            })
            .unwrap();
        app.poll_account_pages();
        assert_eq!(app.view.nodes.len(), 2);
        assert!(!app.view.nodes[0].openable);
        assert_eq!(view_json(&app.view, true)[0]["status"], "pending");
        assert!(requests_rx.try_recv().is_err());
        app.request_account_page();
        assert!(requests_rx.try_recv().is_ok());
        updates_tx
            .send(AccountPageUpdate::Loaded {
                document: document(&[account()]),
                start: 0,
                scanned: 1,
                total: 2,
            })
            .unwrap();
        app.poll_account_pages();
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let detail = app.view.raw.clone();
        app.request_account_page();
        assert!(requests_rx.try_recv().is_err());
        app.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(requests_rx.try_recv().is_ok());
        app.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(requests_rx.try_recv().is_err());

        let mut next = account();
        next.address = "NextAccount".into();
        updates_tx
            .send(AccountPageUpdate::Loaded {
                document: document(&[next]),
                start: 1,
                scanned: 2,
                total: 2,
            })
            .unwrap();
        app.poll_account_pages();
        assert_eq!(app.view.raw, detail);
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.view.nodes.len(), 2);
        assert_eq!(app.view.raw.as_array().unwrap().len(), 2);
        assert_eq!(app.view.nodes[1].label, "NextAccount");
        assert!(app.view.nodes[1].openable);
        assert!(app.view.raw[1].get("status").is_none());
        app.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(requests_rx.try_recv().is_err());
    }

    #[test]
    fn account_pages_wait_for_navigation_and_prefetch_at_each_page_boundary() {
        let (updates_tx, updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let (requests_tx, mut requests_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(account_document(
            &[],
            &account_type(),
            &BTreeMap::new(),
            metadata(),
        ));
        app.paging = Some(AccountPaging::new(updates_rx, requests_tx));
        let addresses = (0..250)
            .map(|index| format!("Account{index}"))
            .collect::<Vec<_>>();
        updates_tx
            .send(AccountPageUpdate::Discovered {
                addresses: addresses.clone(),
                account_type: "State".into(),
            })
            .unwrap();
        app.poll_account_pages();
        app.request_account_page();
        assert!(requests_rx.try_recv().is_ok());
        let send_page = |start: usize, end: usize| {
            let accounts = addresses[start..end]
                .iter()
                .map(|address| DecodedAccount {
                    address: address.clone(),
                    ..account()
                })
                .collect::<Vec<_>>();
            updates_tx
                .send(AccountPageUpdate::Loaded {
                    document: account_document(
                        &accounts,
                        &account_type(),
                        &BTreeMap::new(),
                        metadata(),
                    ),
                    start,
                    scanned: end,
                    total: addresses.len(),
                })
                .unwrap();
        };
        for (start, end) in [(0, 100), (100, 200)] {
            send_page(start, end);
            app.poll_account_pages();
            app.request_account_page();
            assert!(requests_rx.try_recv().is_err());
            app.selected = end - 12;
            app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            assert!(requests_rx.try_recv().is_err());
            app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            assert!(requests_rx.try_recv().is_ok());
            app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            app.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
            assert!(requests_rx.try_recv().is_err());
        }
        send_page(200, 250);
        app.poll_account_pages();
        app.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        app.selected = 248;
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(requests_rx.try_recv().is_err());
    }

    #[test]
    fn large_account_lists_render_the_selected_addresses_before_details_load() {
        let (updates_tx, updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let (requests_tx, mut requests_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(account_document(
            &[],
            &account_type(),
            &BTreeMap::new(),
            metadata(),
        ));
        app.paging = Some(AccountPaging::new(updates_rx, requests_tx));
        updates_tx
            .send(AccountPageUpdate::Discovered {
                addresses: (0..200_000)
                    .map(|index| format!("Account{index:06}"))
                    .collect(),
                account_type: "State".into(),
            })
            .unwrap();
        app.poll_account_pages();
        assert!(requests_rx.try_recv().is_err());
        app.selected = 199_999;
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Account199999"));
        assert!(text.contains("Waiting for details"));
        assert!(!text.contains("Account000000"));
        app.request_account_page();
        assert!(requests_rx.try_recv().is_ok());
    }

    #[test]
    fn failed_account_page_can_be_retried_without_losing_loaded_accounts() {
        let (updates_tx, updates_rx) = tokio::sync::mpsc::unbounded_channel();
        let (requests_tx, mut requests_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(account_document(
            &[account()],
            &account_type(),
            &BTreeMap::new(),
            metadata(),
        ));
        app.paging = Some(AccountPaging::new(updates_rx, requests_tx));
        updates_tx
            .send(AccountPageUpdate::Failed("RPC unavailable".into()))
            .unwrap();
        app.poll_account_pages();
        assert!(
            app.paging
                .as_ref()
                .unwrap()
                .message
                .contains("RPC unavailable")
        );
        assert_eq!(app.view.nodes.len(), 1);
        app.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(requests_rx.try_recv().is_ok());
    }

    #[test]
    fn root_escape_and_control_c_quit() {
        let mut app = App::new(account_document(
            &[account()],
            &account_type(),
            &BTreeMap::new(),
            metadata(),
        ));
        assert!(app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn browser_renders_a_dynamic_header_and_rounded_border() {
        let mut terminal = Terminal::new(TestBackend::new(90, 16)).unwrap();
        let app = App::new(account_document(
            &[account()],
            &account_type(),
            &BTreeMap::new(),
            metadata(),
        ));
        terminal.draw(|frame| app.render(frame)).unwrap();
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .any(|cell| cell.symbol() == "╭")
        );
    }
}
