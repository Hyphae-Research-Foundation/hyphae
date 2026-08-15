// SPDX-License-Identifier: AGPL-3.0-only

//! Interactive native operator console.

use std::{io::stdout, path::PathBuf, time::Duration};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use hyphae_native_product::{
    AccessControlStatus, ProductCapabilities, ProductOperation, ProductResponse, ProductScope,
    ProductValue, SecurityAssignmentListRequest, SecurityAssignmentPage, SecurityAuditPage,
    SecurityAuditReadRequest, SecurityCursor, SecurityId, SecurityKeyListRequest, SecurityKeyPage,
    SecurityPrincipalListRequest, SecurityPrincipalPage, SecurityRoleListRequest, SecurityRolePage,
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Row, Table, Tabs, Wrap},
};

use crate::{exit::CliFailure, native_client::EmbeddedClient};

const MAX_SQL_INPUT_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const SECURITY_PAGE_ROWS: usize = 12;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum View {
    Overview,
    Sql,
    Structures,
    Search,
    Catalog,
    Security,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SecuritySection {
    Status,
    Principals,
    Roles,
    Assignments,
    Keys,
    Audit,
}

impl SecuritySection {
    const ALL: [Self; 6] = [
        Self::Status,
        Self::Principals,
        Self::Roles,
        Self::Assignments,
        Self::Keys,
        Self::Audit,
    ];

    const fn title(self) -> &'static str {
        match self {
            Self::Status => "Status",
            Self::Principals => "Principals",
            Self::Roles => "Roles",
            Self::Assignments => "Assignments",
            Self::Keys => "Keys",
            Self::Audit => "Audit",
        }
    }
}

enum SecurityPanel {
    Message(String),
    Status(AccessControlStatus),
    Principals(SecurityPrincipalPage),
    Roles(SecurityRolePage),
    Assignments(SecurityAssignmentPage),
    Keys(SecurityKeyPage),
    Audit(SecurityAuditPage),
}

struct SecurityState {
    section_index: usize,
    panel: SecurityPanel,
    notice: Option<String>,
}

impl SecurityState {
    fn new(managed: bool, client: &mut EmbeddedClient) -> Self {
        let mut state = Self {
            section_index: 0,
            panel: SecurityPanel::Message(String::new()),
            notice: None,
        };
        if managed {
            state.load(None, None, client);
        } else {
            state.panel = SecurityPanel::Message(
                "Managed API-key session required for the security read plane.".to_owned(),
            );
        }
        state
    }

    const fn section(&self) -> SecuritySection {
        SecuritySection::ALL[self.section_index]
    }

    fn next_section(&mut self, client: &mut EmbeddedClient) {
        self.section_index = (self.section_index + 1) % SecuritySection::ALL.len();
        self.load(None, None, client);
    }

    fn previous_section(&mut self, client: &mut EmbeddedClient) {
        self.section_index = self
            .section_index
            .checked_sub(1)
            .unwrap_or(SecuritySection::ALL.len() - 1);
        self.load(None, None, client);
    }

    fn reload_first(&mut self, client: &mut EmbeddedClient) {
        self.load(None, None, client);
    }

    fn next_page(&mut self, client: &mut EmbeddedClient) {
        let metadata_cursor = match &self.panel {
            SecurityPanel::Principals(page) => page.next_cursor,
            SecurityPanel::Roles(page) => page.next_cursor,
            SecurityPanel::Assignments(page) => page.next_cursor,
            SecurityPanel::Keys(page) => page.next_cursor,
            _ => None,
        };
        let audit_cursor = match &self.panel {
            SecurityPanel::Audit(page) => page.next_cursor,
            _ => None,
        };
        if metadata_cursor.is_none() && audit_cursor.is_none() {
            self.notice = Some("End of the bounded result set.".to_owned());
            return;
        }
        self.load(metadata_cursor, audit_cursor, client);
    }

    fn load(
        &mut self,
        metadata_cursor: Option<SecurityCursor>,
        audit_cursor: Option<SecurityId>,
        client: &mut EmbeddedClient,
    ) {
        self.notice = None;
        let operation = match self.operation(metadata_cursor, audit_cursor) {
            Ok(operation) => operation,
            Err(message) => {
                self.panel = SecurityPanel::Message(message);
                return;
            }
        };
        self.panel = match client.dispatch(operation) {
            Ok(response) => panel_from_response(self.section(), response),
            Err(error) => SecurityPanel::Message(format!("{}: {error}", error.code().as_str())),
        };
    }

    fn operation(
        &self,
        metadata_cursor: Option<SecurityCursor>,
        audit_cursor: Option<SecurityId>,
    ) -> Result<ProductOperation, String> {
        let invalid = |_| "Invalid bounded security request.".to_owned();
        match self.section() {
            SecuritySection::Status => Ok(ProductOperation::SecurityStatus),
            SecuritySection::Principals => {
                SecurityPrincipalListRequest::new(metadata_cursor, SECURITY_PAGE_ROWS)
                    .map(ProductOperation::SecurityPrincipalList)
                    .map_err(invalid)
            }
            SecuritySection::Roles => {
                SecurityRoleListRequest::new(metadata_cursor, SECURITY_PAGE_ROWS)
                    .map(ProductOperation::SecurityRoleList)
                    .map_err(invalid)
            }
            SecuritySection::Assignments => {
                SecurityAssignmentListRequest::new(metadata_cursor, SECURITY_PAGE_ROWS)
                    .map(ProductOperation::SecurityAssignmentList)
                    .map_err(invalid)
            }
            SecuritySection::Keys => {
                SecurityKeyListRequest::new(metadata_cursor, SECURITY_PAGE_ROWS)
                    .map(ProductOperation::SecurityKeyList)
                    .map_err(invalid)
            }
            SecuritySection::Audit => {
                SecurityAuditReadRequest::new(audit_cursor, SECURITY_PAGE_ROWS)
                    .map(ProductOperation::SecurityAuditRead)
                    .map_err(invalid)
            }
        }
    }
}

fn panel_from_response(section: SecuritySection, response: ProductResponse) -> SecurityPanel {
    match (section, response) {
        (SecuritySection::Status, ProductResponse::SecurityStatus(status)) => {
            SecurityPanel::Status(status)
        }
        (SecuritySection::Principals, ProductResponse::SecurityPrincipalPage(page)) => {
            SecurityPanel::Principals(page)
        }
        (SecuritySection::Roles, ProductResponse::SecurityRolePage(page)) => {
            SecurityPanel::Roles(page)
        }
        (SecuritySection::Assignments, ProductResponse::SecurityAssignmentPage(page)) => {
            SecurityPanel::Assignments(page)
        }
        (SecuritySection::Keys, ProductResponse::SecurityKeyPage(page)) => {
            SecurityPanel::Keys(page)
        }
        (SecuritySection::Audit, ProductResponse::SecurityAuditPage(page)) => {
            SecurityPanel::Audit(page)
        }
        _ => SecurityPanel::Message("Unexpected product response for security view.".to_owned()),
    }
}

impl View {
    const ALL: [Self; 6] = [
        Self::Overview,
        Self::Sql,
        Self::Structures,
        Self::Search,
        Self::Catalog,
        Self::Security,
    ];

    const fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Sql => "SQL",
            Self::Structures => "Structures",
            Self::Search => "Search",
            Self::Catalog => "Catalog",
            Self::Security => "Security",
        }
    }
}

struct App {
    data_dir: PathBuf,
    view_index: usize,
    capabilities: ProductCapabilities,
    managed: bool,
    security: SecurityState,
    sql: String,
    output: String,
    should_quit: bool,
}

impl App {
    fn new(data_dir: PathBuf, client: &mut EmbeddedClient) -> Result<Self, CliFailure> {
        let capabilities = client.capabilities()?;
        let managed = client.is_managed();
        let security = SecurityState::new(managed, client);
        Ok(Self {
            data_dir,
            view_index: 0,
            capabilities,
            managed,
            security,
            sql: String::new(),
            output: "Ready. Tab changes view; r refreshes; q exits.".to_owned(),
            should_quit: false,
        })
    }

    const fn view(&self) -> View {
        View::ALL[self.view_index]
    }

    fn next_view(&mut self) {
        self.view_index = (self.view_index + 1) % View::ALL.len();
    }

    fn previous_view(&mut self) {
        self.view_index = self
            .view_index
            .checked_sub(1)
            .unwrap_or(View::ALL.len() - 1);
    }

    fn refresh(&mut self, client: &mut EmbeddedClient) -> Result<(), CliFailure> {
        self.capabilities = client.capabilities()?;
        "Dashboard refreshed within the authenticated authority.".clone_into(&mut self.output);
        Ok(())
    }

    fn execute_sql(&mut self, client: &mut EmbeddedClient) {
        let statement = self.sql.trim();
        if statement.is_empty() {
            "Enter a SQL statement before executing.".clone_into(&mut self.output);
            return;
        }
        let operation = ProductOperation::ExecuteSql {
            statement: statement.to_owned(),
            parameters: Vec::<ProductValue>::new(),
        };
        self.output = match client.dispatch(operation) {
            Ok(response) => bounded_output(
                serde_json::to_string_pretty(&super::response_json(response))
                    .unwrap_or_else(|_| "unable to render response".to_owned()),
            ),
            Err(error) => format!("{}: {}", error.code().as_str(), error),
        };
    }

    fn handle_key(&mut self, key: KeyEvent, client: &mut EmbeddedClient) -> Result<(), CliFailure> {
        if key.kind != KeyEventKind::Press {
            return Ok(());
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return Ok(());
        }
        match key.code {
            KeyCode::Tab | KeyCode::Right => self.next_view(),
            KeyCode::BackTab | KeyCode::Left => self.previous_view(),
            KeyCode::Down if self.view() == View::Security && self.managed => {
                self.security.next_section(client);
            }
            KeyCode::Up if self.view() == View::Security && self.managed => {
                self.security.previous_section(client);
            }
            KeyCode::Char('n') if self.view() == View::Security && self.managed => {
                self.security.next_page(client);
            }
            KeyCode::Char('r') if self.view() == View::Security && self.managed => {
                self.security.reload_first(client);
            }
            KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('q') if self.view() != View::Sql => self.should_quit = true,
            KeyCode::Char('r') if self.view() != View::Sql => self.refresh(client)?,
            KeyCode::Enter if self.view() == View::Sql => self.execute_sql(client),
            KeyCode::Backspace if self.view() == View::Sql => {
                self.sql.pop();
            }
            KeyCode::Char(character)
                if self.view() == View::Sql
                    && self.sql.len() + character.len_utf8() <= MAX_SQL_INPUT_BYTES =>
            {
                self.sql.push(character);
            }
            _ => {}
        }
        Ok(())
    }
}

/// Runs the interactive console while holding exclusive native ownership.
pub(crate) fn run(data_dir: PathBuf, mut client: EmbeddedClient) -> Result<(), CliFailure> {
    let mut app = App::new(data_dir, &mut client)?;
    enable_raw_mode()?;
    let _guard = TerminalModeGuard;
    execute!(stdout(), EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    while !app.should_quit {
        terminal.draw(|frame| render(frame, &app))?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            app.handle_key(key, &mut client)?;
        }
    }
    Ok(())
}

struct TerminalModeGuard;

impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        let _ignored = disable_raw_mode();
        let _ignored = execute!(stdout(), LeaveAlternateScreen);
    }
}

fn render(frame: &mut Frame<'_>, app: &App) {
    let regions = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(3),
        ])
        .split(frame.area());
    render_navigation(frame, app, regions[0]);
    render_body(frame, app, regions[1]);
    render_footer(frame, app, regions[2]);
}

fn render_navigation(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let titles = View::ALL
        .iter()
        .map(|view| Line::from(view.title()))
        .collect::<Vec<_>>();
    let tabs = Tabs::new(titles)
        .select(app.view_index)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" HYPHAE / native console "),
        )
        .highlight_style(
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_widget(tabs, area);
}

fn render_body(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if app.view() == View::Sql {
        render_sql(frame, app, area);
        return;
    }
    if app.view() == View::Security {
        render_security(frame, app, area);
        return;
    }
    let content = match app.view() {
        View::Overview => overview_lines(app),
        View::Structures => placeholder_lines(
            "Structures",
            "Browse strings, hashes, lists, sets, sorted sets, and streams.",
        ),
        View::Search => placeholder_lines(
            "Search",
            "Inspect lexical, vector, ANN, and hybrid retrieval with proofs.",
        ),
        View::Catalog => placeholder_lines(
            "Catalog",
            "Resolve stable objects, dependencies, schemas, and physical bindings.",
        ),
        View::Security | View::Sql => unreachable!("specialized view rendered above"),
    };
    frame.render_widget(
        Paragraph::new(content)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(app.view().title()),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_security(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let regions = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(5)])
        .split(area);
    let titles = SecuritySection::ALL
        .iter()
        .map(|section| Line::from(section.title()))
        .collect::<Vec<_>>();
    frame.render_widget(
        Tabs::new(titles)
            .select(app.security.section_index)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Security read plane "),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            ),
        regions[0],
    );
    match &app.security.panel {
        SecurityPanel::Message(message) => render_security_message(frame, message, regions[1]),
        SecurityPanel::Status(status) => render_security_status(frame, app, *status, regions[1]),
        SecurityPanel::Principals(page) => render_principals(frame, app, page, regions[1]),
        SecurityPanel::Roles(page) => render_roles(frame, app, page, regions[1]),
        SecurityPanel::Assignments(page) => render_assignments(frame, app, page, regions[1]),
        SecurityPanel::Keys(page) => render_keys(frame, app, page, regions[1]),
        SecurityPanel::Audit(page) => render_audit(frame, app, page, regions[1]),
    }
}

fn render_security_message(frame: &mut Frame<'_>, message: &str, area: Rect) {
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(message.to_owned()),
            Line::from(""),
            Line::from("Credential secrets and verifiers are never rendered."),
        ])
        .block(Block::default().borders(Borders::ALL).title(" Security "))
        .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_security_status(
    frame: &mut Frame<'_>,
    app: &App,
    status: AccessControlStatus,
    area: Rect,
) {
    let mut lines = vec![
        metric_line(
            "Session authority",
            if app.managed {
                "managed API key"
            } else {
                "unmanaged pre-bootstrap"
            }
            .to_owned(),
        ),
        metric_line("Bootstrapped", status.bootstrapped.to_string()),
        metric_line("Authorization epoch", status.epoch.get().to_string()),
        metric_line("Principals", status.principals.to_string()),
        metric_line("Assignments", status.assignments.to_string()),
        metric_line("Custom roles", status.custom_roles.to_string()),
        metric_line("Custom assignments", status.custom_assignments.to_string()),
        metric_line("Keys", status.keys.to_string()),
        metric_line("Pending keys", status.pending_keys.to_string()),
        metric_line("Audit events", status.audit_events.to_string()),
    ];
    append_security_notice(&mut lines, app);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" Status "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_principals(frame: &mut Frame<'_>, app: &App, page: &SecurityPrincipalPage, area: Rect) {
    let rows = page
        .items
        .iter()
        .map(|principal| {
            Row::new(vec![
                compact_id(&principal.id()),
                principal.display_name().to_owned(),
                principal.enabled().to_string(),
            ])
        })
        .collect::<Vec<_>>();
    render_security_table(
        frame,
        app,
        rows,
        SecurityTableSpec {
            title: " Principals ",
            headers: ["ID", "Name", "Enabled"],
            widths: [
                Constraint::Length(18),
                Constraint::Min(18),
                Constraint::Length(9),
            ],
            has_next: page.next_cursor.is_some(),
        },
        area,
    );
}

fn render_roles(frame: &mut Frame<'_>, app: &App, page: &SecurityRolePage, area: Rect) {
    let rows = page
        .items
        .iter()
        .map(|role| {
            let (identifier, kind) = role.built_in_role().map_or_else(
                || {
                    (
                        role.custom_role_id()
                            .map_or_else(|| "-".to_owned(), |id| compact_id(&id)),
                        "custom",
                    )
                },
                |built_in| (built_in.as_str().to_owned(), "built-in"),
            );
            Row::new(vec![
                identifier,
                kind.to_owned(),
                role.display_name().to_owned(),
                role.grants().len().to_string(),
            ])
        })
        .collect::<Vec<_>>();
    render_security_table(
        frame,
        app,
        rows,
        SecurityTableSpec {
            title: " Roles ",
            headers: ["ID", "Kind", "Name", "Grants"],
            widths: [
                Constraint::Length(18),
                Constraint::Length(10),
                Constraint::Min(16),
                Constraint::Length(7),
            ],
            has_next: page.next_cursor.is_some(),
        },
        area,
    );
}

fn render_assignments(frame: &mut Frame<'_>, app: &App, page: &SecurityAssignmentPage, area: Rect) {
    let rows = page
        .items
        .iter()
        .map(|assignment| {
            let role = assignment.built_in_role().map_or_else(
                || {
                    assignment
                        .custom_role_id()
                        .map_or_else(|| "-".to_owned(), |id| compact_id(&id))
                },
                |built_in| built_in.as_str().to_owned(),
            );
            Row::new(vec![
                compact_id(&assignment.id()),
                compact_id(&assignment.principal_id()),
                role,
                assignment
                    .scope()
                    .map_or_else(|| "custom".to_owned(), scope_label),
            ])
        })
        .collect::<Vec<_>>();
    render_security_table(
        frame,
        app,
        rows,
        SecurityTableSpec {
            title: " Assignments ",
            headers: ["ID", "Principal", "Role", "Scope"],
            widths: [
                Constraint::Length(18),
                Constraint::Length(18),
                Constraint::Min(12),
                Constraint::Length(18),
            ],
            has_next: page.next_cursor.is_some(),
        },
        area,
    );
}

fn render_keys(frame: &mut Frame<'_>, app: &App, page: &SecurityKeyPage, area: Rect) {
    let rows = page
        .items
        .iter()
        .map(|key| {
            let state = if key.revoked() {
                "revoked"
            } else if key.active() {
                "active"
            } else {
                "pending"
            };
            Row::new(vec![
                compact_id(&key.id()),
                compact_id(&key.principal_id()),
                key.label().to_owned(),
                state.to_owned(),
            ])
        })
        .collect::<Vec<_>>();
    render_security_table(
        frame,
        app,
        rows,
        SecurityTableSpec {
            title: " Keys / redacted metadata only ",
            headers: ["Key ID", "Principal", "Label", "State"],
            widths: [
                Constraint::Length(18),
                Constraint::Length(18),
                Constraint::Min(16),
                Constraint::Length(9),
            ],
            has_next: page.next_cursor.is_some(),
        },
        area,
    );
}

fn render_audit(frame: &mut Frame<'_>, app: &App, page: &SecurityAuditPage, area: Rect) {
    let rows = page
        .events
        .iter()
        .map(|event| {
            Row::new(vec![
                compact_id(&event.id()),
                event.commit_csn().to_string(),
                super::security_audit_action(event.action()).to_owned(),
                event
                    .actor_principal_id()
                    .map_or_else(|| "offline".to_owned(), |id| compact_id(&id)),
                event.targets().len().to_string(),
            ])
        })
        .collect::<Vec<_>>();
    render_security_table(
        frame,
        app,
        rows,
        SecurityTableSpec {
            title: " Audit / redacted events ",
            headers: ["Event", "CSN", "Action", "Actor", "Targets"],
            widths: [
                Constraint::Length(18),
                Constraint::Length(9),
                Constraint::Min(18),
                Constraint::Length(18),
                Constraint::Length(8),
            ],
            has_next: page.next_cursor.is_some(),
        },
        area,
    );
}

#[derive(Clone, Copy)]
struct SecurityTableSpec<const COLUMNS: usize> {
    title: &'static str,
    headers: [&'static str; COLUMNS],
    widths: [Constraint; COLUMNS],
    has_next: bool,
}

fn render_security_table<const COLUMNS: usize>(
    frame: &mut Frame<'_>,
    app: &App,
    rows: Vec<Row<'static>>,
    spec: SecurityTableSpec<COLUMNS>,
    area: Rect,
) {
    let regions = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(area);
    frame.render_widget(
        Table::new(rows, spec.widths)
            .header(
                Row::new(spec.headers).style(
                    Style::default()
                        .fg(Color::LightCyan)
                        .add_modifier(Modifier::BOLD),
                ),
            )
            .block(Block::default().borders(Borders::ALL).title(spec.title)),
        regions[0],
    );
    let page_state = if spec.has_next {
        "more rows: n"
    } else {
        "last page"
    };
    let notice = app.security.notice.as_deref().unwrap_or(page_state);
    frame.render_widget(
        Paragraph::new(Line::from(format!(
            "{notice} · max {SECURITY_PAGE_ROWS} rows"
        )))
        .style(Style::default().fg(Color::DarkGray)),
        regions[1],
    );
}

fn append_security_notice(lines: &mut Vec<Line<'static>>, app: &App) {
    if let Some(notice) = &app.security.notice {
        lines.push(Line::from(""));
        lines.push(Line::from(notice.clone()));
    }
}

fn compact_id(id: &(impl ToString + ?Sized)) -> String {
    let id = id.to_string();
    if id.chars().count() <= 16 {
        return id;
    }
    let prefix = id.chars().take(15).collect::<String>();
    format!("{prefix}…")
}

fn scope_label(scope: ProductScope) -> String {
    match scope {
        ProductScope::Instance => "instance".to_owned(),
        ProductScope::CatalogSubtree(object) => format!("subtree:{}", compact_id(&object)),
        ProductScope::CatalogObject(object) => format!("object:{}", compact_id(&object)),
    }
}

fn render_sql(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let direction = if area.width < 90 {
        Direction::Vertical
    } else {
        Direction::Horizontal
    };
    let panes = Layout::default()
        .direction(direction)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);
    frame.render_widget(
        Paragraph::new(app.sql.as_str())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" SQL input / Enter executes "),
            )
            .wrap(Wrap { trim: false }),
        panes[0],
    );
    frame.render_widget(
        Paragraph::new(app.output.as_str())
            .block(Block::default().borders(Borders::ALL).title(" Result "))
            .wrap(Wrap { trim: false }),
        panes[1],
    );
}

fn render_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let help = if app.view() == View::Sql {
        "Tab/←/→ views  Enter run  Backspace edit  Esc exit"
    } else if app.view() == View::Security {
        "↑/↓ security views  n next  r first  Tab/←/→ views  q/Esc exit"
    } else {
        "Tab/←/→ views  r refresh  q/Esc exit"
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " exact local authority ",
                Style::default().fg(Color::Black).bg(Color::LightCyan),
            ),
            Span::raw(help),
        ]))
        .block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn overview_lines(app: &App) -> Vec<Line<'static>> {
    let access_control = if app.managed {
        "managed API-key session".to_owned()
    } else {
        "unmanaged / pre-bootstrap".to_owned()
    };
    vec![
        metric_line("Data directory", app.data_dir.display().to_string()),
        metric_line(
            "Product API",
            app.capabilities.product_api_version.to_string(),
        ),
        metric_line(
            "Directory format",
            app.capabilities.native_directory_format.to_string(),
        ),
        metric_line("SQL row limit", app.capabilities.max_sql_rows.to_string()),
        metric_line("Access control", access_control),
        Line::from(""),
        Line::from("One binary. One directory. SQL + structures + lexical/vector search."),
    ]
}

fn placeholder_lines(title: &'static str, description: &'static str) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            title,
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(description),
        Line::from(
            "The view is wired to the native product client; interactive actions land next.",
        ),
    ]
}

fn metric_line(label: &'static str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label:>22}  "),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(value, Style::default().fg(Color::White)),
    ])
}

fn bounded_output(mut output: String) -> String {
    if output.len() <= MAX_OUTPUT_BYTES {
        return output;
    }
    let mut boundary = MAX_OUTPUT_BYTES;
    while !output.is_char_boundary(boundary) {
        boundary -= 1;
    }
    output.truncate(boundary);
    output.push_str("\n… output truncated by console bound");
    output
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        error::Error,
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use hyphae_native_product::{
        ApiKeyId, AuthorizationEpoch, BuiltInRole, NativeProduct, ProductAuthorization,
        SecurityCursorId, SecurityKeySummary, SecurityKeySummaryInput,
    };
    use ratatui::{Terminal, backend::TestBackend};

    static NEXT_MANAGED_FIXTURE: AtomicU64 = AtomicU64::new(1);

    struct ManagedTuiFixture {
        root: PathBuf,
        data: PathBuf,
        owner_key: PathBuf,
        reader_key: PathBuf,
        owner_secret: String,
        reader_secret: String,
    }

    impl ManagedTuiFixture {
        fn create() -> Result<Self, Box<dyn Error>> {
            let sequence = NEXT_MANAGED_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "hyphae-tui-managed-{}-{sequence}",
                std::process::id()
            ));
            let _ignored = fs::remove_dir_all(&root);
            fs::create_dir_all(&root)?;
            let data = root.join("data");
            let owner_key = root.join("owner.key");
            let reader_key = root.join("reader.key");
            let mut product = NativeProduct::create(&data)?;
            product.bootstrap_access_control_to_file("Owner", "owner", &owner_key, 1)?;
            let owner_secret = fs::read_to_string(&owner_key)?;
            add_principals(&mut product, &owner_secret)?;
            add_reader(&mut product, &owner_secret, &reader_key)?;
            let reader_secret = fs::read_to_string(&reader_key)?;
            drop(product);
            Ok(Self {
                root,
                data,
                owner_key,
                reader_key,
                owner_secret,
                reader_secret,
            })
        }

        fn owner_client(&self) -> Result<EmbeddedClient, Box<dyn Error>> {
            self.client(&self.owner_key)
        }

        fn reader_client(&self) -> Result<EmbeddedClient, Box<dyn Error>> {
            self.client(&self.reader_key)
        }

        fn client(&self, key: &Path) -> Result<EmbeddedClient, Box<dyn Error>> {
            Ok(EmbeddedClient::open(
                NativeProduct::open(&self.data)?,
                Some(key),
                false,
            )?)
        }
    }

    impl Drop for ManagedTuiFixture {
        fn drop(&mut self) {
            let _ignored = fs::remove_dir_all(&self.root);
        }
    }

    fn add_principals(
        product: &mut NativeProduct,
        owner_secret: &str,
    ) -> Result<(), Box<dyn Error>> {
        for index in 0_i64..13 {
            let owner = product.authenticate_api_key(owner_secret, 0)?;
            product.create_security_principal(
                &owner,
                &format!("TUI principal {index:02}"),
                10 + index,
            )?;
        }
        Ok(())
    }

    fn add_reader(
        product: &mut NativeProduct,
        owner_secret: &str,
        reader_key: &Path,
    ) -> Result<(), Box<dyn Error>> {
        let owner = product.authenticate_api_key(owner_secret, 0)?;
        let reader = product.create_security_principal(&owner, "TUI reader", 30)?;
        let owner = product.authenticate_api_key(owner_secret, 0)?;
        product.assign_built_in_role(
            &owner,
            reader.principal_id,
            BuiltInRole::Reader,
            ProductScope::Instance,
            31,
        )?;
        let owner = product.authenticate_api_key(owner_secret, 0)?;
        product.set_security_principal_enabled(&owner, reader.principal_id, true, 32)?;
        let owner = product.authenticate_api_key(owner_secret, 0)?;
        product.issue_api_key_to_file(
            &owner,
            reader.principal_id,
            "tui-reader",
            [BuiltInRole::Reader],
            BuiltInRole::Reader.authorization(),
            None,
            reader_key,
            33,
        )?;
        Ok(())
    }

    fn status_panel() -> SecurityPanel {
        SecurityPanel::Status(AccessControlStatus {
            bootstrapped: true,
            epoch: AuthorizationEpoch::new(7),
            principals: 3,
            assignments: 4,
            custom_roles: 1,
            custom_assignments: 1,
            keys: 2,
            pending_keys: 0,
            audit_events: 9,
        })
    }

    fn security_state(section: SecuritySection, panel: SecurityPanel) -> SecurityState {
        SecurityState {
            section_index: SecuritySection::ALL
                .iter()
                .position(|candidate| *candidate == section)
                .unwrap_or(0),
            panel,
            notice: None,
        }
    }

    fn fixture(width: u16, height: u16, view: View) -> Result<String, Infallible> {
        fixture_with_security(
            width,
            height,
            view,
            security_state(SecuritySection::Status, status_panel()),
        )
    }

    fn fixture_with_security(
        width: u16,
        height: u16,
        view: View,
        security: SecurityState,
    ) -> Result<String, Infallible> {
        let app = App {
            data_dir: PathBuf::from("/var/lib/hyphae"),
            view_index: View::ALL
                .iter()
                .position(|candidate| *candidate == view)
                .unwrap_or(0),
            capabilities: hyphae_native_product::capabilities(),
            managed: true,
            security,
            sql: "SELECT value FROM items WHERE id = ?".to_owned(),
            output: "ready".to_owned(),
            should_quit: false,
        };
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| render(frame, &app))?;
        Ok(terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>())
    }

    fn render_app(width: u16, height: u16, app: &App) -> Result<String, Infallible> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| render(frame, app))?;
        Ok(terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>())
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn enter_security(app: &mut App, client: &mut EmbeddedClient) -> Result<(), CliFailure> {
        for _ in 0..5 {
            app.handle_key(press(KeyCode::Tab), client)?;
        }
        Ok(())
    }

    fn assert_secret_absent(rendered: &str, secret: &str) {
        assert!(!rendered.contains(secret.trim()));
        let fragment = secret.trim().rsplit('_').next().unwrap_or(secret.trim());
        assert!(!rendered.contains(fragment));
        assert!(!rendered.contains("hyp1_"));
    }

    #[test]
    fn dashboard_renders_at_compact_and_wide_sizes_without_secrets() -> Result<(), Infallible> {
        for (width, height) in [(80, 24), (120, 36), (200, 60)] {
            let rendered = fixture(width, height, View::Security)?;
            assert!(rendered.contains("managed API key"));
            assert!(rendered.contains("Authorization epoch"));
            assert!(rendered.contains("Audit events"));
            assert!(!rendered.contains("hyp1_"));
        }
        Ok(())
    }

    #[test]
    fn managed_navigation_uses_emitted_cursor_and_refreshes_first_page()
    -> Result<(), Box<dyn Error>> {
        let fixture = ManagedTuiFixture::create()?;
        let mut client = fixture.owner_client()?;
        let mut app = App::new(fixture.data.clone(), &mut client)?;
        enter_security(&mut app, &mut client)?;
        assert_eq!(app.view(), View::Security);
        app.handle_key(press(KeyCode::Down), &mut client)?;
        let (first_id, continuation) = match &app.security.panel {
            SecurityPanel::Principals(page) => (
                page.items.first().ok_or("empty first principal page")?.id(),
                page.next_cursor.ok_or("missing principal continuation")?,
            ),
            _ => return Err("security Down did not load principals".into()),
        };
        assert!(matches!(
            continuation.after_id(),
            SecurityCursorId::Principal(_)
        ));

        app.handle_key(press(KeyCode::Char('n')), &mut client)?;
        let second_id = match &app.security.panel {
            SecurityPanel::Principals(page) => page
                .items
                .first()
                .ok_or("empty second principal page")?
                .id(),
            _ => return Err("security n did not load the continuation".into()),
        };
        assert_ne!(first_id, second_id);

        app.handle_key(press(KeyCode::Char('r')), &mut client)?;
        let refreshed_id = match &app.security.panel {
            SecurityPanel::Principals(page) => page
                .items
                .first()
                .ok_or("empty refreshed principal page")?
                .id(),
            _ => return Err("security r did not reload the first page".into()),
        };
        assert_eq!(refreshed_id, first_id);
        app.handle_key(press(KeyCode::Up), &mut client)?;
        assert_eq!(app.security.section(), SecuritySection::Status);
        assert!(matches!(app.security.panel, SecurityPanel::Status(_)));
        Ok(())
    }

    #[test]
    fn managed_denial_stays_typed_inside_the_security_panel() -> Result<(), Box<dyn Error>> {
        let fixture = ManagedTuiFixture::create()?;
        let mut client = fixture.reader_client()?;
        let mut app = App::new(fixture.data.clone(), &mut client)?;
        assert!(matches!(
            &app.security.panel,
            SecurityPanel::Message(message) if message.contains("authorization_denied")
        ));
        enter_security(&mut app, &mut client)?;
        app.handle_key(press(KeyCode::Down), &mut client)?;
        let SecurityPanel::Message(message) = &app.security.panel else {
            return Err("denied reader received a security page".into());
        };
        assert!(message.contains("authorization_denied"));
        let rendered = render_app(80, 24, &app)?;
        assert!(rendered.contains("authorization_denied"));
        assert_secret_absent(&rendered, &fixture.owner_secret);
        assert_secret_absent(&rendered, &fixture.reader_secret);
        Ok(())
    }

    #[test]
    fn real_managed_security_views_are_responsive_and_secret_safe() -> Result<(), Box<dyn Error>> {
        let fixture = ManagedTuiFixture::create()?;
        let mut client = fixture.owner_client()?;
        let mut app = App::new(fixture.data.clone(), &mut client)?;
        enter_security(&mut app, &mut client)?;
        let markers = [
            "Authorization epoch",
            "TUI principal",
            "built-in",
            "owner",
            "redacted metadata only",
            "bootstrap_owner",
        ];
        for (index, marker) in markers.into_iter().enumerate() {
            assert_eq!(app.security.section(), SecuritySection::ALL[index]);
            if app.security.section() == SecuritySection::Keys {
                assert!(matches!(
                    &app.security.panel,
                    SecurityPanel::Keys(page) if !page.items.is_empty()
                ));
            }
            if app.security.section() == SecuritySection::Audit {
                assert!(matches!(
                    &app.security.panel,
                    SecurityPanel::Audit(page) if !page.events.is_empty()
                ));
            }
            for (width, height) in [(80, 24), (120, 36), (200, 60)] {
                let rendered = render_app(width, height, &app)?;
                assert!(rendered.contains(marker));
                assert_secret_absent(&rendered, &fixture.owner_secret);
                assert_secret_absent(&rendered, &fixture.reader_secret);
                assert!(!rendered.to_ascii_lowercase().contains("verifier"));
            }
            if index + 1 < SecuritySection::ALL.len() {
                app.handle_key(press(KeyCode::Down), &mut client)?;
            }
        }
        Ok(())
    }

    #[test]
    fn security_navigation_snapshots_all_bounded_read_views() -> Result<(), Infallible> {
        let epoch = AuthorizationEpoch::new(7);
        let cases = [
            (
                SecuritySection::Principals,
                SecurityPanel::Principals(SecurityPrincipalPage {
                    authorization_epoch: epoch,
                    items: Box::new([]),
                    next_cursor: None,
                }),
                "Name",
            ),
            (
                SecuritySection::Roles,
                SecurityPanel::Roles(SecurityRolePage {
                    authorization_epoch: epoch,
                    items: Box::new([]),
                    next_cursor: None,
                }),
                "Kind",
            ),
            (
                SecuritySection::Assignments,
                SecurityPanel::Assignments(SecurityAssignmentPage {
                    authorization_epoch: epoch,
                    items: Box::new([]),
                    next_cursor: None,
                }),
                "Principal",
            ),
            (
                SecuritySection::Audit,
                SecurityPanel::Audit(SecurityAuditPage {
                    events: Box::new([]),
                    next_cursor: None,
                }),
                "Targets",
            ),
        ];
        for (section, panel, marker) in cases {
            let rendered =
                fixture_with_security(120, 30, View::Security, security_state(section, panel))?;
            assert!(rendered.contains(section.title()));
            assert!(rendered.contains(marker));
            assert!(rendered.contains("max 12 rows"));
        }
        Ok(())
    }

    #[test]
    fn key_table_renders_only_redacted_metadata() -> Result<(), Box<dyn Error>> {
        let key = SecurityKeySummary::try_from_wire(SecurityKeySummaryInput {
            id: ApiKeyId::from_bytes([0x11; 16]).ok_or("zero key id")?,
            principal_id: SecurityId::new(0x22).ok_or("zero principal id")?,
            label: "release-operator".to_owned(),
            active: true,
            roles: vec![BuiltInRole::Owner],
            custom_roles: Vec::new(),
            permission_ceiling: ProductAuthorization::ALL,
            scope_ceiling: vec![ProductScope::Instance],
            created_at_micros: 1,
            expires_at_micros: None,
            revoked: false,
            published_epoch: AuthorizationEpoch::INITIAL,
            predecessor_id: None,
            successor_id: None,
            overlap_until_micros: None,
            rotation_overlap_micros: None,
        })?;
        let rendered = fixture_with_security(
            120,
            30,
            View::Security,
            security_state(
                SecuritySection::Keys,
                SecurityPanel::Keys(SecurityKeyPage {
                    authorization_epoch: AuthorizationEpoch::INITIAL,
                    items: vec![key].into_boxed_slice(),
                    next_cursor: None,
                }),
            ),
        )?;
        assert!(rendered.contains("release-operator"));
        assert!(rendered.contains("active"));
        assert!(rendered.contains("redacted metadata only"));
        for forbidden in ["hyp1_", "verifier", "credential", "secret"] {
            assert!(!rendered.to_ascii_lowercase().contains(forbidden));
        }
        Ok(())
    }

    #[test]
    fn security_operations_are_read_only_and_page_bounded() -> Result<(), Box<dyn Error>> {
        let epoch = AuthorizationEpoch::new(7);
        let key_id = ApiKeyId::from_bytes([0x33; 16]).ok_or("zero key id")?;
        for (section, cursor) in [
            (
                SecuritySection::Principals,
                SecurityCursor::new(
                    epoch,
                    SecurityCursorId::Principal(SecurityId::new(1).ok_or("zero id")?),
                ),
            ),
            (
                SecuritySection::Roles,
                SecurityCursor::new(epoch, SecurityCursorId::BuiltInRole(BuiltInRole::Reader)),
            ),
            (
                SecuritySection::Assignments,
                SecurityCursor::new(
                    epoch,
                    SecurityCursorId::Assignment(SecurityId::new(2).ok_or("zero id")?),
                ),
            ),
            (
                SecuritySection::Keys,
                SecurityCursor::new(epoch, SecurityCursorId::Key(key_id)),
            ),
        ] {
            let state = security_state(section, status_panel());
            let operation = state.operation(Some(cursor), None)?;
            let (returned_cursor, limit) = match operation {
                ProductOperation::SecurityPrincipalList(request) => {
                    (request.cursor(), request.limit())
                }
                ProductOperation::SecurityRoleList(request) => (request.cursor(), request.limit()),
                ProductOperation::SecurityAssignmentList(request) => {
                    (request.cursor(), request.limit())
                }
                ProductOperation::SecurityKeyList(request) => (request.cursor(), request.limit()),
                _ => return Err("security table emitted a non-read operation".into()),
            };
            assert_eq!(returned_cursor, Some(cursor));
            assert_eq!(limit, SECURITY_PAGE_ROWS);
        }

        let audit = security_state(SecuritySection::Audit, status_panel())
            .operation(None, SecurityId::new(2))?;
        let ProductOperation::SecurityAuditRead(request) = audit else {
            return Err("audit table emitted a non-read operation".into());
        };
        assert_eq!(request.cursor(), SecurityId::new(2));
        assert_eq!(request.limit(), SECURITY_PAGE_ROWS);
        Ok(())
    }

    #[test]
    fn sql_layout_switches_for_narrow_terminals() -> Result<(), Infallible> {
        for (width, height) in [(80, 24), (120, 36)] {
            let rendered = fixture(width, height, View::Sql)?;
            assert!(rendered.contains("SQL input"));
            assert!(rendered.contains("Result"));
            assert!(rendered.contains("SELECT value"));
        }
        Ok(())
    }

    #[test]
    fn output_bound_preserves_utf8_boundary() {
        let output = "á".repeat(MAX_OUTPUT_BYTES);
        let bounded = bounded_output(output);
        assert!(bounded.is_char_boundary(bounded.len()));
        assert!(bounded.ends_with("output truncated by console bound"));
    }
}
