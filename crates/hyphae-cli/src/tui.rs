// SPDX-License-Identifier: AGPL-3.0-only

//! Interactive native operator console.

use std::{io::stdout, path::PathBuf, time::Duration};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use hyphae_native_product::{
    AccessControlStatus, NativeProduct, ProductCapabilities, ProductOperation, ProductValue,
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Tabs, Wrap},
};

use crate::{exit::CliFailure, native_client::EmbeddedClient};

const MAX_SQL_INPUT_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum View {
    Overview,
    Sql,
    Structures,
    Search,
    Catalog,
    Security,
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
    security: AccessControlStatus,
    sql: String,
    output: String,
    should_quit: bool,
}

impl App {
    fn new(data_dir: PathBuf, client: &mut EmbeddedClient) -> Result<Self, CliFailure> {
        let capabilities = client.product_mut().capabilities();
        let security = client.product_mut().access_control_status()?;
        Ok(Self {
            data_dir,
            view_index: 0,
            capabilities,
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
        self.security = client.product_mut().access_control_status()?;
        "Dashboard refreshed from the native product authority.".clone_into(&mut self.output);
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
pub(crate) fn run(data_dir: PathBuf) -> Result<(), CliFailure> {
    let product = NativeProduct::open(&data_dir)?;
    let mut client = EmbeddedClient::new(product)?;
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
        View::Security => security_lines(app),
        View::Sql => unreachable!("SQL rendered above"),
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
        metric_line(
            "Access control",
            if app.security.bootstrapped {
                format!("active / epoch {}", app.security.epoch.get())
            } else {
                "not bootstrapped".to_owned()
            },
        ),
        Line::from(""),
        Line::from("One binary. One directory. SQL + structures + lexical/vector search."),
    ]
}

fn security_lines(app: &App) -> Vec<Line<'static>> {
    vec![
        metric_line("Bootstrapped", app.security.bootstrapped.to_string()),
        metric_line("Authorization epoch", app.security.epoch.get().to_string()),
        metric_line("Principals", app.security.principals.to_string()),
        metric_line("Assignments", app.security.assignments.to_string()),
        metric_line("Key records", app.security.keys.to_string()),
        metric_line("Pending outputs", app.security.pending_keys.to_string()),
        Line::from(""),
        Line::from("Credential secrets are never rendered in this console."),
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
    use std::convert::Infallible;

    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn fixture(width: u16, height: u16, view: View) -> Result<String, Infallible> {
        let mut app = App {
            data_dir: PathBuf::from("/var/lib/hyphae"),
            view_index: View::ALL
                .iter()
                .position(|candidate| *candidate == view)
                .unwrap_or(0),
            capabilities: hyphae_native_product::capabilities(),
            security: AccessControlStatus {
                bootstrapped: true,
                epoch: hyphae_native_product::AuthorizationEpoch::new(7),
                principals: 3,
                assignments: 4,
                keys: 5,
                pending_keys: 0,
            },
            sql: "SELECT value FROM items WHERE id = ?".to_owned(),
            output: "ready".to_owned(),
            should_quit: false,
        };
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| render(frame, &app))?;
        app.should_quit = true;
        Ok(terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>())
    }

    #[test]
    fn dashboard_renders_at_compact_and_wide_sizes_without_secrets() -> Result<(), Infallible> {
        for (width, height) in [(80, 24), (120, 36), (200, 60)] {
            let rendered = fixture(width, height, View::Security)?;
            assert!(rendered.contains("Authorization epoch"));
            assert!(rendered.contains("Credential secrets are never rendered"));
            assert!(!rendered.contains("hyp1_"));
        }
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
