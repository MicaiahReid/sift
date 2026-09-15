//! Interactive presentation for decoded account-list results.

use std::{
    collections::BTreeMap,
    io::IsTerminal,
    time::{Duration, Instant},
};

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
use serde_json::{Map, Value};

/// One successfully decoded account suitable for either JSON or terminal rendering.
#[derive(Clone, Debug)]
pub(crate) struct DecodedAccount {
    pub(crate) address: String,
    pub(crate) account_type: String,
    pub(crate) data: Value,
    pub(crate) seeds: Option<String>,
}

/// Query details rendered above the browser so results remain attributable to their source.
#[derive(Clone, Debug)]
pub(crate) struct RequestMetadata {
    pub(crate) account_type: String,
    pub(crate) program: String,
    pub(crate) idl_location: String,
    pub(crate) rpc_url: String,
    pub(crate) commitment: String,
}

/// Converts decoded account records to the stable JSON list contract.
pub(crate) fn account_list_json(accounts: &[DecodedAccount]) -> Value {
    Value::Array(accounts.iter().map(DecodedAccount::as_json).collect())
}

impl DecodedAccount {
    fn as_json(&self) -> Value {
        let mut account = Map::new();
        account.insert("address".to_owned(), Value::String(self.address.clone()));
        account.insert("type".to_owned(), Value::String(self.account_type.clone()));
        account.insert("data".to_owned(), self.data.clone());
        if let Some(seeds) = &self.seeds {
            account.insert("seeds".to_owned(), Value::String(seeds.clone()));
        }
        Value::Object(account)
    }
}

/// Returns whether the interactive account browser can be shown for this invocation.
pub(crate) fn should_run(no_tui: bool, has_accounts: bool) -> bool {
    !no_tui && has_accounts && std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Starts the account browser and restores the caller's terminal before returning.
pub(crate) fn run(
    accounts: &[DecodedAccount],
    account_type: &Value,
    definitions: &BTreeMap<String, Value>,
    metadata: RequestMetadata,
) -> Result<()> {
    let mut terminal = ratatui::try_init().context("cannot initialize the account browser")?;
    let mut app = App::new(
        accounts.to_vec(),
        account_type.clone(),
        definitions.clone(),
        metadata,
    );
    let result = app.run(&mut terminal);
    let restore =
        ratatui::try_restore().context("cannot restore the terminal after the account browser");
    restore?;
    result
}

#[derive(Clone, Debug)]
enum View {
    Accounts,
    Children {
        title: String,
        value: Value,
        ty: Value,
    },
    Bytes {
        title: String,
        value: Value,
        bytes: Vec<u8>,
    },
    Scalar {
        title: String,
        value: Value,
        ty: Value,
    },
}

#[derive(Debug)]
struct RawView {
    text: String,
    scroll: u16,
}

#[derive(Clone, Debug)]
struct Node {
    label: String,
    value: Value,
    ty: Value,
}

#[derive(Debug)]
struct App {
    accounts: Vec<DecodedAccount>,
    account_type: Value,
    definitions: BTreeMap<String, Value>,
    metadata: RequestMetadata,
    view: View,
    history: Vec<(View, usize)>,
    selected: usize,
    raw: Option<RawView>,
    status: Option<(String, Instant)>,
}

impl App {
    fn new(
        accounts: Vec<DecodedAccount>,
        account_type: Value,
        definitions: BTreeMap<String, Value>,
        metadata: RequestMetadata,
    ) -> Self {
        Self {
            accounts,
            account_type,
            definitions,
            metadata,
            view: View::Accounts,
            history: Vec::new(),
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
                .is_some_and(|(_, shown_at)| shown_at.elapsed() > Duration::from_secs(3))
            {
                self.status = None;
            }
            terminal.draw(|frame| self.render(frame))?;
            if event::poll(Duration::from_millis(250))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
                && self.handle_key(key)
            {
                return Ok(());
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return true;
        }
        if self.raw.is_some() {
            let mut copied_text = None;
            let raw = self.raw.as_mut().expect("checked above");
            match key.code {
                KeyCode::Char('q') => return true,
                KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('r') => self.raw = None,
                KeyCode::Up => raw.scroll = raw.scroll.saturating_sub(1),
                KeyCode::Down => raw.scroll = raw.scroll.saturating_add(1),
                KeyCode::PageUp => raw.scroll = raw.scroll.saturating_sub(10),
                KeyCode::PageDown => raw.scroll = raw.scroll.saturating_add(10),
                KeyCode::Char('c') => copied_text = Some(raw.text.clone()),
                _ => {}
            }
            if let Some(text) = copied_text {
                self.copy_text(text);
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
                let count = self.item_count();
                if count > 0 {
                    self.selected = (self.selected + 1).min(count - 1);
                }
                false
            }
            KeyCode::Enter => {
                self.descend();
                false
            }
            KeyCode::Esc if self.history.is_empty() => true,
            KeyCode::Esc | KeyCode::Backspace => {
                self.back();
                false
            }
            KeyCode::Char('r') => {
                self.raw = Some(RawView {
                    text: self.current_raw_text(),
                    scroll: 0,
                });
                false
            }
            KeyCode::Char('c') => {
                self.copy_text(self.current_raw_text());
                false
            }
            _ => false,
        }
    }

    fn item_count(&self) -> usize {
        match &self.view {
            View::Accounts => self.accounts.len(),
            View::Children { value, ty, .. } => child_nodes(value, ty, &self.definitions).len(),
            View::Bytes { .. } | View::Scalar { .. } => 0,
        }
    }

    fn descend(&mut self) {
        let next = match &self.view {
            View::Accounts => self
                .accounts
                .get(self.selected)
                .map(|account| View::Children {
                    title: format!("{} ({})", account.address, account.account_type),
                    value: account.data.clone(),
                    ty: self.account_type.clone(),
                }),
            View::Children { value, ty, .. } => child_nodes(value, ty, &self.definitions)
                .get(self.selected)
                .cloned()
                .map(|node| self.view_for_node(node)),
            View::Bytes { .. } | View::Scalar { .. } => None,
        };
        if let Some(next) = next {
            self.history.push((self.view.clone(), self.selected));
            self.view = next;
            self.selected = 0;
        }
    }

    fn view_for_node(&self, node: Node) -> View {
        if byte_array(&node.value, &node.ty, &self.definitions).is_some() {
            return View::Bytes {
                title: format!(
                    "{}: {}",
                    node.label,
                    type_label(&node.ty, &self.definitions)
                ),
                bytes: byte_array(&node.value, &node.ty, &self.definitions).unwrap_or_default(),
                value: node.value,
            };
        }
        if child_nodes(&node.value, &node.ty, &self.definitions).is_empty() {
            View::Scalar {
                title: format!(
                    "{}: {}",
                    node.label,
                    type_label(&node.ty, &self.definitions)
                ),
                value: node.value,
                ty: node.ty,
            }
        } else {
            View::Children {
                title: format!(
                    "{}: {}",
                    node.label,
                    type_label(&node.ty, &self.definitions)
                ),
                value: node.value,
                ty: node.ty,
            }
        }
    }

    fn back(&mut self) {
        if let Some((view, selected)) = self.history.pop() {
            self.view = view;
            self.selected = selected;
        }
    }

    fn current_raw_value(&self) -> Value {
        match &self.view {
            View::Accounts => account_list_json(&self.accounts),
            View::Children { value, .. }
            | View::Bytes { value, .. }
            | View::Scalar { value, .. } => value.clone(),
        }
    }

    fn current_raw_text(&self) -> String {
        serde_json::to_string_pretty(&self.current_raw_value())
            .unwrap_or_else(|error| format!("<cannot render JSON: {error}>"))
    }

    fn copy_text(&mut self, text: String) {
        self.status = Some((
            match Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text)) {
                Ok(()) => "Copied JSON to clipboard".to_owned(),
                Err(error) => format!("Could not copy to clipboard: {error}"),
            },
            Instant::now(),
        ));
    }

    fn render(&self, frame: &mut Frame) {
        let areas = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(5),
            Constraint::Length(0),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(frame.area());
        self.render_metadata(frame, areas[1]);
        if let Some(raw) = &self.raw {
            self.render_raw(frame, areas[3], raw);
            self.render_footer(
                frame,
                areas[4],
                "↑/↓ scroll  c copy  r/Esc back  q/Ctrl-C quit",
            );
            return;
        }
        match &self.view {
            View::Accounts => self.render_accounts(frame, areas[3]),
            View::Children { title, value, ty } => {
                self.render_children(frame, areas[3], title, value, ty)
            }
            View::Bytes { title, bytes, .. } => self.render_bytes(frame, areas[3], title, bytes),
            View::Scalar { title, value, ty } => {
                self.render_scalar(frame, areas[3], title, value, ty)
            }
        }
        self.render_footer(
            frame,
            areas[4],
            "↑/↓ move  Enter open  r raw  c copy  Esc back/quit  q/Ctrl-C quit",
        );
    }

    fn render_metadata(&self, frame: &mut Frame, area: Rect) {
        let label = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD);
        let value = Style::default().fg(Color::Cyan);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title("Request");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let rows = Layout::vertical([Constraint::Length(1); 3]).split(inner);
        let columns = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]);
        let first = columns.split(rows[0]);
        let second = columns.split(rows[1]);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Account type: ", label),
                Span::styled(&self.metadata.account_type, value),
            ])),
            first[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Program: ", label),
                Span::styled(&self.metadata.program, value),
            ]))
            .alignment(Alignment::Right),
            first[1],
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("RPC: ", label),
                Span::styled(&self.metadata.rpc_url, value),
            ])),
            second[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Commitment: ", label),
                Span::styled(&self.metadata.commitment, value),
            ]))
            .alignment(Alignment::Right),
            second[1],
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("IDL: ", label),
                Span::styled(&self.metadata.idl_location, value),
            ])),
            rows[2],
        );
    }

    fn render_accounts(&self, frame: &mut Frame, area: Rect) {
        let items = self.accounts.iter().map(|account| {
            let mut spans = vec![Span::styled(
                &account.address,
                Style::default().fg(Color::Cyan),
            )];
            if let Some(seeds) = &account.seeds {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(seeds, Style::default().fg(Color::Yellow)));
            }
            ListItem::new(Line::from(spans))
        });
        self.render_list(
            frame,
            area,
            "Decoded accounts",
            items.collect(),
            self.selected,
        );
    }

    fn render_children(
        &self,
        frame: &mut Frame,
        area: Rect,
        title: &str,
        value: &Value,
        ty: &Value,
    ) {
        let preview_width = usize::from(area.width.saturating_sub(36)).max(12);
        let items = child_nodes(value, ty, &self.definitions)
            .into_iter()
            .map(|node| {
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{:<18}", node.label),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled(
                        truncate(&type_label(&node.ty, &self.definitions), 16),
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::raw("  "),
                    Span::raw(preview(
                        &node.value,
                        &node.ty,
                        &self.definitions,
                        preview_width,
                    )),
                ]))
            })
            .collect();
        self.render_list(frame, area, title, items, self.selected);
    }

    fn render_bytes(&self, frame: &mut Frame, area: Rect, title: &str, bytes: &[u8]) {
        let text = format!("{} byte(s)\n\n{}", bytes.len(), hex_dump(bytes));
        frame.render_widget(
            Paragraph::new(text)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .title(title),
                )
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn render_scalar(&self, frame: &mut Frame, area: Rect, title: &str, value: &Value, ty: &Value) {
        let value = serde_json::to_string_pretty(value).unwrap_or_else(|error| error.to_string());
        let text = format!("{}\n\n{}", type_label(ty, &self.definitions), value);
        frame.render_widget(
            Paragraph::new(text)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .title(title),
                )
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn render_list(
        &self,
        frame: &mut Frame,
        area: Rect,
        title: &str,
        items: Vec<ListItem>,
        selected: usize,
    ) {
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .title(title),
            )
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("› ");
        let mut state = ListState::default();
        state.select(Some(selected));
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn render_raw(&self, frame: &mut Frame, area: Rect, raw: &RawView) {
        frame.render_widget(
            Paragraph::new(raw.text.as_str())
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .title("Raw decoded JSON"),
                )
                .scroll((raw.scroll, 0))
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect, help: &str) {
        let message = self.status.as_ref().map_or(help, |(message, _)| message);
        frame.render_widget(
            Paragraph::new(message).style(Style::default().fg(Color::DarkGray)),
            area,
        );
    }
}

fn child_nodes(value: &Value, ty: &Value, definitions: &BTreeMap<String, Value>) -> Vec<Node> {
    let ty = resolve_type(ty, definitions);
    if ty.get("kind").and_then(Value::as_str) == Some("struct") {
        return ty
            .get("fields")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|field| {
                let label = field.get("name").and_then(Value::as_str)?;
                Some(Node {
                    label: label.to_owned(),
                    value: value.get(label).cloned().unwrap_or(Value::Null),
                    ty: field.get("type").cloned().unwrap_or_else(|| field.clone()),
                })
            })
            .collect();
    }
    if let Some(values) = value.as_array() {
        let element = array_element_type(ty).unwrap_or_else(|| Value::String("unknown".to_owned()));
        return values
            .iter()
            .enumerate()
            .map(|(index, value)| Node {
                label: format!("[{index}]"),
                value: value.clone(),
                ty: element.clone(),
            })
            .collect();
    }
    value
        .as_object()
        .map(|values| {
            values
                .iter()
                .map(|(label, value)| Node {
                    label: label.clone(),
                    value: value.clone(),
                    ty: Value::String("unknown".to_owned()),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn resolve_type<'a>(mut ty: &'a Value, definitions: &'a BTreeMap<String, Value>) -> &'a Value {
    for _ in 0..16 {
        let name = match ty {
            Value::String(name) if definitions.contains_key(name) => Some(name.as_str()),
            Value::Object(object) => object
                .get("defined")
                .and_then(Value::as_str)
                .or_else(|| {
                    object
                        .get("defined")
                        .and_then(|value| value.get("name"))
                        .and_then(Value::as_str)
                })
                .or_else(|| {
                    (object.get("kind").and_then(Value::as_str) == Some("defined"))
                        .then(|| object.get("name").and_then(Value::as_str))
                        .flatten()
                }),
            _ => None,
        };
        match name.and_then(|name| definitions.get(name)) {
            Some(next) => ty = next,
            None => return ty,
        }
    }
    ty
}

fn array_element_type(ty: &Value) -> Option<Value> {
    let object = ty.as_object()?;
    object
        .get("vec")
        .cloned()
        .or_else(|| object.get("option").cloned())
        .or_else(|| {
            object
                .get("array")
                .and_then(Value::as_array)
                .and_then(|array| array.first())
                .cloned()
        })
}

fn byte_array(value: &Value, ty: &Value, definitions: &BTreeMap<String, Value>) -> Option<Vec<u8>> {
    let ty = resolve_type(ty, definitions);
    let is_bytes = ty.as_str() == Some("bytes")
        || array_element_type(ty)
            .is_some_and(|element| resolve_type(&element, definitions).as_str() == Some("u8"));
    if !is_bytes {
        return None;
    }
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

fn type_label(ty: &Value, definitions: &BTreeMap<String, Value>) -> String {
    let original = ty;
    let ty = resolve_type(ty, definitions);
    if let Some(name) = original.get("defined").and_then(Value::as_str).or_else(|| {
        original
            .get("defined")
            .and_then(|value| value.get("name"))
            .and_then(Value::as_str)
    }) {
        return name.to_owned();
    }
    if let Some(name) = ty.as_str() {
        return name.to_owned();
    }
    if let Some(object) = ty.as_object() {
        if let Some(inner) = object.get("vec") {
            return format!("Vec<{}>", type_label(inner, definitions));
        }
        if let Some(inner) = object.get("option") {
            return format!("Option<{}>", type_label(inner, definitions));
        }
        if let Some(array) = object.get("array").and_then(Value::as_array)
            && let Some(inner) = array.first()
        {
            let length = array
                .get(1)
                .map(Value::to_string)
                .unwrap_or_else(|| "?".to_owned());
            return format!("[{}; {length}]", type_label(inner, definitions));
        }
        if object.get("kind").and_then(Value::as_str) == Some("struct") {
            return "struct".to_owned();
        }
        if object.get("kind").and_then(Value::as_str) == Some("enum") {
            return "enum".to_owned();
        }
    }
    serde_json::to_string(ty).unwrap_or_else(|_| "unknown".to_owned())
}

fn preview(
    value: &Value,
    ty: &Value,
    definitions: &BTreeMap<String, Value>,
    width: usize,
) -> String {
    let text = if let Some(bytes) = byte_array(value, ty, definitions) {
        let preview = bytes
            .iter()
            .take(8)
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        if bytes.len() > 8 {
            format!("{} bytes [{preview} …]", bytes.len())
        } else {
            format!("{} bytes [{preview}]", bytes.len())
        }
    } else {
        serde_json::to_string(value).unwrap_or_else(|_| "<unrenderable>".to_owned())
    };
    truncate(&text.replace('\n', " "), width)
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
    use serde_json::json;

    fn account() -> DecodedAccount {
        DecodedAccount {
            address: "Example111111111111111111111111111111111".to_owned(),
            account_type: "State".to_owned(),
            data: json!({"authority":"11111111111111111111111111111111","bytes":[65,66,0],"count":"7"}),
            seeds: None,
        }
    }

    fn account_type() -> Value {
        json!({"kind":"struct","fields":[
            {"name":"authority","type":"pubkey"},
            {"name":"bytes","type":{"vec":"u8"}},
            {"name":"count","type":"u64"}
        ]})
    }

    fn metadata() -> RequestMetadata {
        RequestMetadata {
            account_type: "State".to_owned(),
            program: "Program111111111111111111111111111111111".to_owned(),
            idl_location: "~/work/example/target/idl/example.json".to_owned(),
            rpc_url: "https://api.devnet.solana.com".to_owned(),
            commitment: "confirmed".to_owned(),
        }
    }

    #[test]
    fn list_json_omits_unknown_seeds() {
        assert_eq!(
            account_list_json(&[account()]),
            json!([{"address":"Example111111111111111111111111111111111","type":"State","data":{"authority":"11111111111111111111111111111111","bytes":[65,66,0],"count":"7"}}])
        );
    }

    #[test]
    fn byte_vectors_are_compact_and_open_a_hex_view() {
        let node = child_nodes(&account().data, &account_type(), &BTreeMap::new())
            .into_iter()
            .find(|node| node.label == "bytes")
            .unwrap();
        assert_eq!(
            byte_array(&node.value, &node.ty, &BTreeMap::new()),
            Some(vec![65, 66, 0])
        );
        assert_eq!(
            preview(&node.value, &node.ty, &BTreeMap::new(), 80),
            "3 bytes [41 42 00]"
        );
        let dump = hex_dump(&[65, 66, 0]);
        assert!(dump.starts_with("Offset    Hex bytes"));
        assert!(dump.contains("ASCII"));
        assert!(dump.contains("41 42 00"));
        assert_eq!(
            byte_array(&json!([65, 66, 0]), &json!("bytes"), &BTreeMap::new()),
            Some(vec![65, 66, 0])
        );
    }

    #[test]
    fn raw_scope_follows_the_current_view() {
        let mut app = App::new(vec![account()], account_type(), BTreeMap::new(), metadata());
        assert!(app.current_raw_text().contains("Example111"));
        app.descend();
        assert_eq!(app.current_raw_value(), account().data);
        app.selected = 2;
        app.descend();
        assert_eq!(app.current_raw_value(), json!("7"));
    }

    #[test]
    fn escape_quits_at_root_and_control_c_quits_anywhere() {
        let mut app = App::new(vec![account()], account_type(), BTreeMap::new(), metadata());
        assert!(app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));

        app.descend();
        assert!(!app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(matches!(app.view, View::Accounts));
        assert!(app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL,)));
    }

    #[test]
    fn root_view_renders_pubkeys() {
        let mut terminal = Terminal::new(TestBackend::new(90, 16)).unwrap();
        let app = App::new(vec![account()], account_type(), BTreeMap::new(), metadata());
        terminal.draw(|frame| app.render(frame)).unwrap();
        let rendered = terminal.backend().buffer().content();
        assert!(rendered.iter().any(|cell| cell.symbol() == "E"));
        assert!(rendered.iter().any(|cell| cell.symbol() == "I"));
        assert!(rendered.iter().any(|cell| cell.symbol() == "╭"));
    }
}
