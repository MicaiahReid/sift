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
use sift_core::Registry;
use sift_types::IdlSource;
use std::{
    collections::BTreeMap,
    io::IsTerminal,
    time::{Duration, Instant},
};

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
                .sources
                .iter()
                .enumerate()
                .map(|(index, source)| source_node(index, source))
                .collect::<Vec<_>>();
            Node {
                label: address.clone(),
                ty: "program".into(),
                preview: format!(
                    "{}, {}",
                    count_label(aliases.len(), "alias"),
                    count_label(sources.len(), "source")
                ),
                raw: json!({"address":address,"aliases":aliases,"sources":program.sources}),
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
fn source_node(index: usize, source: &IdlSource) -> Node {
    let (ty, location) = match source {
        IdlSource::File { path, .. } => ("file", path.display().to_string()),
        IdlSource::Url { url, .. } => ("url", url.to_string()),
        IdlSource::OnChain { address, .. } => (
            "on-chain",
            address.clone().unwrap_or_else(|| "canonical IDL".into()),
        ),
    };
    let raw = serde_json::to_value(source).expect("source serializes");
    Node {
        label: format!("[{}] {ty}", index + 1),
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
            source_field(
                "Address",
                json!(address),
                address.clone().unwrap_or_else(|| "canonical IDL".into()),
            ),
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
    let mut terminal = ratatui::try_init().context("cannot initialize the interactive browser")?;
    let result = App::new(document).run(&mut terminal);
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
        }
    }
    fn run(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        loop {
            if self
                .status
                .as_ref()
                .is_some_and(|(_, time)| time.elapsed() > Duration::from_secs(3))
            {
                self.status = None;
            }
            terminal.draw(|frame| self.render(frame))?;
            if event::poll(Duration::from_millis(250))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
                && self.key(key)
            {
                return Ok(());
            }
        }
    }
    fn key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return true;
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
                    text: raw_text(&self.view.raw),
                    scroll: 0,
                });
                false
            }
            KeyCode::Char('c') => {
                self.copy(raw_text(&self.view.raw));
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
        self.history.push((self.view.clone(), self.selected));
        self.selected = 0;
        self.view = View {
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
        let width = usize::from(area.width.saturating_sub(36)).max(12);
        let items: Vec<_> = self
            .view
            .nodes
            .iter()
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
                        truncate(&node.preview, width),
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
        state.select(Some(self.selected));
        frame.render_stateful_widget(list, area, &mut state);
    }
    fn footer(&self, frame: &mut Frame, area: Rect, help: &str) {
        frame.render_widget(
            Paragraph::new(self.status.as_ref().map_or(help, |(message, _)| message))
                .style(Style::default().fg(Color::DarkGray)),
            area,
        )
    }
}

fn account_children(value: &Value, ty: &Value, defs: &BTreeMap<String, Value>) -> Vec<Node> {
    let ty = resolve_type(ty, defs);
    if ty.get("kind").and_then(Value::as_str) == Some("struct") {
        return ty
            .get("fields")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|field| {
                let name = field.get("name").and_then(Value::as_str)?;
                let field_ty = field.get("type").cloned().unwrap_or_else(|| field.clone());
                Some(account_node(
                    name,
                    value.get(name).unwrap_or(&Value::Null),
                    &field_ty,
                    defs,
                ))
            })
            .collect();
    }
    if let Some(values) = value.as_array() {
        let element = array_element(ty).unwrap_or_else(|| Value::String("unknown".into()));
        return values
            .iter()
            .enumerate()
            .map(|(index, value)| account_node(&format!("[{index}]"), value, &element, defs))
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
        ty: type_label(ty, defs),
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
fn resolve_type<'a>(mut ty: &'a Value, defs: &'a BTreeMap<String, Value>) -> &'a Value {
    for _ in 0..16 {
        let name = match ty {
            Value::String(name) if defs.contains_key(name) => Some(name.as_str()),
            Value::Object(object) => object.get("defined").and_then(Value::as_str).or_else(|| {
                object
                    .get("defined")
                    .and_then(|v| v.get("name"))
                    .and_then(Value::as_str)
            }),
            _ => None,
        };
        match name.and_then(|name| defs.get(name)) {
            Some(next) => ty = next,
            None => return ty,
        }
    }
    ty
}
fn array_element(ty: &Value) -> Option<Value> {
    ty.as_object()?.get("vec").cloned().or_else(|| {
        ty.get("array")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .cloned()
    })
}
fn byte_array(value: &Value, ty: &Value, defs: &BTreeMap<String, Value>) -> Option<Vec<u8>> {
    let ty = resolve_type(ty, defs);
    let byte_type = ty.as_str() == Some("bytes")
        || array_element(ty)
            .is_some_and(|element| resolve_type(&element, defs).as_str() == Some("u8"));
    byte_type.then_some(())?;
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
fn type_label(ty: &Value, defs: &BTreeMap<String, Value>) -> String {
    if let Some(name) = ty.get("defined").and_then(Value::as_str).or_else(|| {
        ty.get("defined")
            .and_then(|v| v.get("name"))
            .and_then(Value::as_str)
    }) {
        return name.into();
    }
    let ty = resolve_type(ty, defs);
    if let Some(name) = ty.as_str() {
        return name.into();
    }
    if let Some(inner) = ty.get("vec") {
        return format!("Vec<{}>", type_label(inner, defs));
    }
    if ty.get("kind").and_then(Value::as_str) == Some("struct") {
        return "struct".into();
    }
    serde_json::to_string(ty).unwrap_or_else(|_| "unknown".into())
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
        json!({"kind":"struct","fields":[{"name":"bytes","type":{"vec":"u8"}},{"name":"count","type":"u64"}]})
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
