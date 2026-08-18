// SPDX-License-Identifier: Apache-2.0

//! Interactive native operator console.

use std::{
    io::stdout,
    path::PathBuf,
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    thread::{self, JoinHandle},
    time::Duration,
};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use hyphae_native_product::proof::NativeProofGenerationLimits;
use hyphae_native_product::{
    AccessControlStatus, BackupRequest, BoundedSearchQuery, CatalogCursor, CatalogListRequest,
    CatalogVisibleCursor, CatalogVisibleListFilter, CatalogVisibleListRequest, ObjectId,
    ProductCancellationToken, ProductCapabilities, ProductExplicitTransactionStatus, ProductLimits,
    ProductOperation, ProductResponse, ProductScope, ProductTransactionHandle,
    ProductTransactionSqlMutation, ProductValue, RestoreRequest, SecurityAssignmentListRequest,
    SecurityAssignmentPage, SecurityAuditPage, SecurityAuditReadRequest, SecurityCursor,
    SecurityId, SecurityKeyListRequest, SecurityKeyPage, SecurityPrincipalListRequest,
    SecurityPrincipalPage, SecurityRoleListRequest, SecurityRolePage,
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
const MAX_FIELD_INPUT_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_SEARCH_QUERY_BYTES: usize = 4 * 1024;
const MAX_PATH_INPUT_BYTES: usize = 4 * 1024;
const CATALOG_PAGE_ROWS: usize = 20;
const CATALOG_VISIT_LIMIT: usize = 256;
const CATALOG_BYTE_LIMIT: usize = 64 * 1024;
const SEARCH_RESULT_LIMIT: usize = 20;
const SECURITY_PAGE_ROWS: usize = 12;
const CONTROLLER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const CONSOLE_QUERY_TIMEOUT_SECONDS: u64 = 30;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum View {
    Overview,
    Sql,
    Structures,
    Search,
    Catalog,
    Backups,
    Operations,
    Proofs,
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
    fn new(managed: bool) -> Self {
        Self {
            section_index: 0,
            panel: SecurityPanel::Message(if managed {
                "Press r to load the bounded security status.".to_owned()
            } else {
                "Managed API-key session required for the security read plane.".to_owned()
            }),
            notice: None,
        }
    }

    const fn section(&self) -> SecuritySection {
        SecuritySection::ALL[self.section_index]
    }

    fn next_section(&mut self) {
        self.section_index = (self.section_index + 1) % SecuritySection::ALL.len();
    }

    fn previous_section(&mut self) {
        self.section_index = self
            .section_index
            .checked_sub(1)
            .unwrap_or(SecuritySection::ALL.len() - 1);
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
    const ALL: [Self; 9] = [
        Self::Overview,
        Self::Sql,
        Self::Structures,
        Self::Search,
        Self::Catalog,
        Self::Backups,
        Self::Operations,
        Self::Proofs,
        Self::Security,
    ];

    const fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Sql => "SQL",
            Self::Structures => "Structures",
            Self::Search => "Search",
            Self::Catalog => "Catalog",
            Self::Backups => "Backups",
            Self::Operations => "Operations",
            Self::Proofs => "Proofs",
            Self::Security => "Security",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestKind {
    Overview,
    Sql,
    Structures,
    Search,
    Catalog,
    Backup,
    Restore,
    Operations,
    Transaction,
    ProofGenerate,
    ProofVerify,
    Security(SecuritySection),
}

struct OperationTask {
    operation: ProductOperation,
    cancellation: ProductCancellationToken,
}

type OperationResult = Result<ProductResponse, Box<hyphae_native_product::ProductError>>;

struct ActiveOperation {
    kind: RequestKind,
    label: String,
    cancellation: ProductCancellationToken,
}

struct CompletedOperation {
    active: ActiveOperation,
    result: OperationResult,
}

struct OperationController {
    requests: Option<SyncSender<OperationTask>>,
    results: Receiver<OperationResult>,
    worker: Option<JoinHandle<()>>,
    worker_stopped: Receiver<()>,
    active: Option<ActiveOperation>,
}

impl OperationController {
    fn new(mut client: EmbeddedClient) -> Self {
        Self::with_dispatch(move |task| {
            client.dispatch_bounded(task.operation, task.cancellation, console_product_limits())
        })
    }

    fn with_dispatch(
        mut dispatch: impl FnMut(OperationTask) -> OperationResult + Send + 'static,
    ) -> Self {
        let (request_tx, request_rx) = mpsc::sync_channel::<OperationTask>(1);
        let (result_tx, result_rx) = mpsc::sync_channel::<OperationResult>(1);
        let (stopped_tx, stopped_rx) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            while let Ok(task) = request_rx.recv() {
                let result = dispatch(task);
                if result_tx.send(result).is_err() {
                    break;
                }
            }
            let _ignored = stopped_tx.send(());
        });
        Self {
            requests: Some(request_tx),
            results: result_rx,
            worker: Some(worker),
            worker_stopped: stopped_rx,
            active: None,
        }
    }

    fn submit(
        &mut self,
        kind: RequestKind,
        label: impl Into<String>,
        operation: ProductOperation,
    ) -> Result<(), &'static str> {
        if self.active.is_some() {
            return Err("Backpressure: one operation is already active (queue capacity 1).");
        }
        let cancellation = ProductCancellationToken::new();
        let task = OperationTask {
            operation,
            cancellation: cancellation.clone(),
        };
        let Some(requests) = &self.requests else {
            return Err("The operation worker is unavailable.");
        };
        match requests.try_send(task) {
            Ok(()) => {
                self.active = Some(ActiveOperation {
                    kind,
                    label: label.into(),
                    cancellation,
                });
                Ok(())
            }
            Err(TrySendError::Full(_)) => {
                Err("Backpressure: the operation queue is full (capacity 1).")
            }
            Err(TrySendError::Disconnected(_)) => Err("The operation worker is unavailable."),
        }
    }

    fn poll(&mut self) -> Option<CompletedOperation> {
        match self.results.try_recv() {
            Ok(result) => self
                .active
                .take()
                .map(|active| CompletedOperation { active, result }),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }

    fn cancel(&self) -> bool {
        let Some(active) = &self.active else {
            return false;
        };
        active.cancellation.cancel();
        true
    }

    fn active_label(&self) -> Option<&str> {
        self.active.as_ref().map(|active| active.label.as_str())
    }

    fn request_shutdown(&mut self) -> bool {
        if self.active.is_none() {
            self.requests.take();
            true
        } else {
            self.cancel();
            false
        }
    }
}

impl Drop for OperationController {
    fn drop(&mut self) {
        if let Some(active) = &self.active {
            active.cancellation.cancel();
        }
        self.requests.take();
        if self
            .worker_stopped
            .recv_timeout(CONTROLLER_SHUTDOWN_TIMEOUT)
            .is_ok()
            && let Some(worker) = self.worker.take()
        {
            let _ignored = worker.join();
        }
    }
}

const fn console_product_limits() -> ProductLimits {
    ProductLimits {
        max_count: 256,
        max_request_bytes: 1024 * 1024,
        max_response_bytes: 8 * 1024 * 1024,
        max_work_units: 100_000,
        max_memory_bytes: 16 * 1024 * 1024,
    }
}

struct Confirmation {
    kind: RequestKind,
    label: String,
    operation: ProductOperation,
}

#[derive(Default)]
struct ProofArtifact {
    proof: Vec<u8>,
    witness: Vec<u8>,
    anchor: [u8; 32],
}

struct App {
    data_dir: PathBuf,
    view_index: usize,
    capabilities: ProductCapabilities,
    managed: bool,
    security: SecurityState,
    sql: String,
    structure_key: String,
    structure_value: String,
    structure_field: usize,
    search_index: String,
    search_query: String,
    search_field: usize,
    catalog_cursor: Option<CatalogCursor>,
    catalog_visible_cursor: Option<CatalogVisibleCursor>,
    backup_source: String,
    backup_destination: String,
    backup_field: usize,
    transaction_sql: String,
    transaction_handle: Option<ProductTransactionHandle>,
    proof_artifact: Option<ProofArtifact>,
    confirmation: Option<Confirmation>,
    active_operation: Option<String>,
    output: String,
    shutdown_pending: bool,
    should_quit: bool,
}

impl App {
    fn new(data_dir: PathBuf, client: &mut EmbeddedClient) -> Result<Self, CliFailure> {
        let capabilities = client.capabilities()?;
        let managed = client.is_managed();
        let security = SecurityState::new(managed);
        Ok(Self {
            data_dir,
            view_index: 0,
            capabilities,
            managed,
            security,
            sql: String::new(),
            structure_key: String::new(),
            structure_value: String::new(),
            structure_field: 0,
            search_index: String::new(),
            search_query: String::new(),
            search_field: 0,
            catalog_cursor: None,
            catalog_visible_cursor: None,
            backup_source: String::new(),
            backup_destination: String::new(),
            backup_field: 0,
            transaction_sql: String::new(),
            transaction_handle: None,
            proof_artifact: None,
            confirmation: None,
            active_operation: None,
            output: "Ready. Tab changes workspace; q exits.".to_owned(),
            shutdown_pending: false,
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

    fn submit(
        &mut self,
        controller: &mut OperationController,
        kind: RequestKind,
        label: impl Into<String>,
        operation: ProductOperation,
    ) -> bool {
        let label = label.into();
        match controller.submit(kind, label.clone(), operation) {
            Ok(()) => {
                self.active_operation = Some(label);
                "Operation admitted; input queue capacity is now exhausted."
                    .clone_into(&mut self.output);
                true
            }
            Err(message) => {
                message.clone_into(&mut self.output);
                false
            }
        }
    }

    fn confirm(
        &mut self,
        kind: RequestKind,
        label: impl Into<String>,
        operation: ProductOperation,
    ) {
        let label = label.into();
        self.output = format!("Confirm {label}? y executes; n cancels.");
        self.confirmation = Some(Confirmation {
            kind,
            label,
            operation,
        });
    }

    fn execute_sql(&mut self, controller: &mut OperationController) {
        let statement = self.sql.trim();
        if statement.is_empty() {
            "Enter a SQL statement before executing.".clone_into(&mut self.output);
            return;
        }
        let operation = ProductOperation::ExecuteSql {
            statement: statement.to_owned(),
            parameters: Vec::<ProductValue>::new(),
        };
        if operation.is_read_only() {
            let _accepted = self.submit(controller, RequestKind::Sql, "SQL", operation);
        } else {
            self.confirm(RequestKind::Sql, "SQL mutation", operation);
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle_key(&mut self, key: KeyEvent, controller: &mut OperationController) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if controller.request_shutdown() {
                self.should_quit = true;
            } else {
                self.shutdown_pending = true;
                self.active_operation = controller
                    .active_label()
                    .map(|label| format!("Cancelling {label}"));
                "Cancellation requested; exit is refused until the operation cooperates."
                    .clone_into(&mut self.output);
            }
            return;
        }
        if self.confirmation.is_some() {
            match key.code {
                KeyCode::Char('y' | 'Y') => {
                    if let Some(confirmation) = self.confirmation.take() {
                        let _accepted = self.submit(
                            controller,
                            confirmation.kind,
                            confirmation.label,
                            confirmation.operation,
                        );
                    }
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    self.confirmation = None;
                    "Operation cancelled before admission.".clone_into(&mut self.output);
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Tab | KeyCode::Right => self.next_view(),
            KeyCode::BackTab | KeyCode::Left => self.previous_view(),
            KeyCode::Down if self.view() == View::Security && self.managed => {
                self.load_adjacent_security(controller, true);
            }
            KeyCode::Up if self.view() == View::Security && self.managed => {
                self.load_adjacent_security(controller, false);
            }
            KeyCode::Char('n') if self.view() == View::Security && self.managed => {
                self.next_security_page(controller);
            }
            KeyCode::Char('r') if self.view() == View::Security && self.managed => {
                self.load_security(controller, None, None);
            }
            KeyCode::Esc => {
                if controller.request_shutdown() {
                    self.should_quit = true;
                } else {
                    self.shutdown_pending = true;
                    self.active_operation = controller
                        .active_label()
                        .map(|label| format!("Cancelling {label}"));
                    "Cancellation requested; exit is refused until the operation cooperates."
                        .clone_into(&mut self.output);
                }
            }
            KeyCode::Char('q')
                if !matches!(
                    self.view(),
                    View::Sql | View::Structures | View::Search | View::Backups | View::Operations
                ) =>
            {
                if controller.active_label().is_some() {
                    "An operation is active; Esc requests cancellation."
                        .clone_into(&mut self.output);
                } else {
                    self.should_quit = true;
                }
            }
            KeyCode::Char('r') if self.view() == View::Overview => {
                let _accepted = self.submit(
                    controller,
                    RequestKind::Overview,
                    "capability refresh",
                    ProductOperation::Capabilities,
                );
            }
            KeyCode::Enter if self.view() == View::Sql => self.execute_sql(controller),
            _ if self.handle_structures_key(key.code, controller) => {}
            _ if self.handle_search_key(key.code, controller) => {}
            _ if self.handle_catalog_key(key.code, controller) => {}
            _ if self.handle_backups_key(key.code, controller) => {}
            _ if self.handle_operations_key(key.code, controller) => {}
            _ if self.handle_proofs_key(key.code, controller) => {}
            KeyCode::Backspace if self.view() == View::Sql => {
                self.sql.pop();
            }
            KeyCode::Char(character) if self.view() == View::Sql => {
                push_bounded(&mut self.sql, character, MAX_SQL_INPUT_BYTES);
            }
            _ => {}
        }
    }
}

impl App {
    fn load_adjacent_security(&mut self, controller: &mut OperationController, next: bool) {
        let previous = self.security.section_index;
        if next {
            self.security.next_section();
        } else {
            self.security.previous_section();
        }
        let section = self.security.section();
        let operation = self.security.operation(None, None);
        let accepted = match operation {
            Ok(operation) => self.submit(
                controller,
                RequestKind::Security(section),
                format!("security {}", section.title()),
                operation,
            ),
            Err(message) => {
                self.security.panel = SecurityPanel::Message(message);
                false
            }
        };
        if !accepted {
            self.security.section_index = previous;
        }
    }

    fn load_security(
        &mut self,
        controller: &mut OperationController,
        metadata_cursor: Option<SecurityCursor>,
        audit_cursor: Option<SecurityId>,
    ) {
        self.security.notice = None;
        match self.security.operation(metadata_cursor, audit_cursor) {
            Ok(operation) => {
                let _accepted = self.submit(
                    controller,
                    RequestKind::Security(self.security.section()),
                    format!("security {}", self.security.section().title()),
                    operation,
                );
            }
            Err(message) => self.security.panel = SecurityPanel::Message(message),
        }
    }

    fn next_security_page(&mut self, controller: &mut OperationController) {
        let metadata_cursor = match &self.security.panel {
            SecurityPanel::Principals(page) => page.next_cursor,
            SecurityPanel::Roles(page) => page.next_cursor,
            SecurityPanel::Assignments(page) => page.next_cursor,
            SecurityPanel::Keys(page) => page.next_cursor,
            _ => None,
        };
        let audit_cursor = match &self.security.panel {
            SecurityPanel::Audit(page) => page.next_cursor,
            _ => None,
        };
        if metadata_cursor.is_none() && audit_cursor.is_none() {
            self.security.notice = Some("End of the bounded result set.".to_owned());
            return;
        }
        self.load_security(controller, metadata_cursor, audit_cursor);
    }

    fn handle_structures_key(
        &mut self,
        code: KeyCode,
        controller: &mut OperationController,
    ) -> bool {
        if self.view() != View::Structures {
            return false;
        }
        match code {
            KeyCode::Up | KeyCode::Down => self.structure_field = 1 - self.structure_field,
            KeyCode::Enter | KeyCode::F(5) => {
                if self.structure_key.is_empty() {
                    "Enter an exact key before reading.".clone_into(&mut self.output);
                } else {
                    let _accepted = self.submit(
                        controller,
                        RequestKind::Structures,
                        "structure get",
                        ProductOperation::StructureGet {
                            key: self.structure_key.as_bytes().to_vec(),
                        },
                    );
                }
            }
            KeyCode::F(6) => {
                if self.structure_key.is_empty() {
                    "Enter an exact key before reading TTL.".clone_into(&mut self.output);
                } else {
                    let _accepted = self.submit(
                        controller,
                        RequestKind::Structures,
                        "structure TTL",
                        ProductOperation::StructureTtl {
                            key: self.structure_key.as_bytes().to_vec(),
                        },
                    );
                }
            }
            KeyCode::F(7) => {
                if self.structure_key.is_empty() {
                    "Enter an exact key before setting a value.".clone_into(&mut self.output);
                } else {
                    self.confirm(
                        RequestKind::Structures,
                        "strict structure set",
                        ProductOperation::StructureSet {
                            key: self.structure_key.as_bytes().to_vec(),
                            value: self.structure_value.as_bytes().to_vec(),
                            expires_at_micros: None,
                        },
                    );
                }
            }
            KeyCode::Backspace => {
                active_structure_input(self).pop();
            }
            KeyCode::Char(character) => {
                let field = active_structure_input(self);
                push_bounded(field, character, MAX_FIELD_INPUT_BYTES);
            }
            _ => {}
        }
        true
    }

    fn handle_search_key(&mut self, code: KeyCode, controller: &mut OperationController) -> bool {
        if self.view() != View::Search {
            return false;
        }
        match code {
            KeyCode::Up | KeyCode::Down => self.search_field = 1 - self.search_field,
            KeyCode::Enter => {
                let Some(index) = parse_object_id(&self.search_index) else {
                    "Enter a nonzero decimal search index ID.".clone_into(&mut self.output);
                    return true;
                };
                if self.search_query.is_empty() {
                    "Enter a lexical query before searching.".clone_into(&mut self.output);
                    return true;
                }
                let _accepted = self.submit(
                    controller,
                    RequestKind::Search,
                    "bounded lexical search",
                    ProductOperation::Search {
                        index,
                        query: BoundedSearchQuery::Term(self.search_query.clone()),
                        limit: SEARCH_RESULT_LIMIT,
                    },
                );
            }
            KeyCode::Backspace => {
                active_search_input(self).pop();
            }
            KeyCode::Char(character) => {
                let limit = if self.search_field == 0 {
                    39
                } else {
                    MAX_SEARCH_QUERY_BYTES
                };
                let field = active_search_input(self);
                push_bounded(field, character, limit);
            }
            _ => {}
        }
        true
    }

    fn handle_catalog_key(&mut self, code: KeyCode, controller: &mut OperationController) -> bool {
        if self.view() != View::Catalog {
            return false;
        }
        match code {
            KeyCode::F(5) => {
                if self.submit_catalog_page(controller, None, None) {
                    self.catalog_cursor = None;
                    self.catalog_visible_cursor = None;
                }
            }
            KeyCode::F(6)
                if self.catalog_cursor.is_some() || self.catalog_visible_cursor.is_some() =>
            {
                self.submit_catalog_page(
                    controller,
                    self.catalog_cursor,
                    self.catalog_visible_cursor.clone(),
                );
            }
            KeyCode::F(6) => {
                "The current bounded catalog page has no continuation."
                    .clone_into(&mut self.output);
            }
            _ => {}
        }
        true
    }

    fn submit_catalog_page(
        &mut self,
        controller: &mut OperationController,
        catalog_cursor: Option<CatalogCursor>,
        catalog_visible_cursor: Option<CatalogVisibleCursor>,
    ) -> bool {
        let operation = if self.managed {
            ProductOperation::CatalogVisibleList(CatalogVisibleListRequest {
                filter: CatalogVisibleListFilter {
                    parent: None,
                    kind: None,
                },
                cursor: catalog_visible_cursor,
                item_limit: CATALOG_PAGE_ROWS,
                visit_limit: CATALOG_VISIT_LIMIT,
                byte_limit: CATALOG_BYTE_LIMIT,
            })
        } else {
            ProductOperation::CatalogList(CatalogListRequest {
                parent: None,
                kind: None,
                cursor: catalog_cursor,
                item_limit: CATALOG_PAGE_ROWS,
                visit_limit: CATALOG_VISIT_LIMIT,
                byte_limit: CATALOG_BYTE_LIMIT,
            })
        };
        self.submit(
            controller,
            RequestKind::Catalog,
            "bounded catalog page",
            operation,
        )
    }

    fn handle_backups_key(&mut self, code: KeyCode, _controller: &mut OperationController) -> bool {
        if self.view() != View::Backups {
            return false;
        }
        match code {
            KeyCode::Up | KeyCode::Down => self.backup_field = 1 - self.backup_field,
            KeyCode::F(5) => match BackupRequest::new(self.backup_destination.trim()) {
                Ok(request) => self.confirm(
                    RequestKind::Backup,
                    "backup create and promoted-backup verification",
                    ProductOperation::Backup(request),
                ),
                Err(error) => self.output = format!("Invalid backup destination: {error}"),
            },
            KeyCode::F(6) => {
                match RestoreRequest::new(self.backup_source.trim(), self.backup_destination.trim())
                {
                    Ok(request) => self.confirm(
                        RequestKind::Restore,
                        "restore into a new data directory",
                        ProductOperation::Restore(request),
                    ),
                    Err(error) => self.output = format!("Invalid restore paths: {error}"),
                }
            }
            KeyCode::Backspace => {
                active_backup_input(self).pop();
            }
            KeyCode::Char(character) => {
                let field = active_backup_input(self);
                push_bounded(field, character, MAX_PATH_INPUT_BYTES);
            }
            _ => {}
        }
        true
    }

    fn handle_operations_key(
        &mut self,
        code: KeyCode,
        controller: &mut OperationController,
    ) -> bool {
        if self.view() != View::Operations {
            return false;
        }
        match code {
            KeyCode::F(5) => {
                let _accepted = self.submit(
                    controller,
                    RequestKind::Operations,
                    "administration status",
                    ProductOperation::AdminStatus,
                );
            }
            KeyCode::F(6) => {
                let _accepted = self.submit(
                    controller,
                    RequestKind::Operations,
                    "bounded telemetry",
                    ProductOperation::Telemetry,
                );
            }
            KeyCode::F(7) => self.confirm(
                RequestKind::Operations,
                "strict checkpoint",
                ProductOperation::AdminCheckpoint,
            ),
            KeyCode::F(8) => self.confirm(
                RequestKind::Transaction,
                "explicit transaction begin",
                ProductOperation::TransactionBegin,
            ),
            KeyCode::F(9) => {
                let Some(handle) = self.transaction_handle else {
                    "Begin a transaction before staging SQL.".clone_into(&mut self.output);
                    return true;
                };
                if self.transaction_sql.trim().is_empty() {
                    "Enter one SQL DML statement before staging.".clone_into(&mut self.output);
                    return true;
                }
                self.confirm(
                    RequestKind::Transaction,
                    "transaction SQL stage",
                    ProductOperation::TransactionStageSql {
                        handle,
                        mutation: ProductTransactionSqlMutation {
                            statement: self.transaction_sql.trim().to_owned(),
                            parameters: Vec::new(),
                        },
                    },
                );
            }
            KeyCode::F(10) => {
                let Some(handle) = self.transaction_handle else {
                    "No active transaction to commit.".clone_into(&mut self.output);
                    return true;
                };
                self.confirm(
                    RequestKind::Transaction,
                    "explicit transaction commit",
                    ProductOperation::TransactionCommit { handle },
                );
            }
            KeyCode::F(11) => {
                let Some(handle) = self.transaction_handle else {
                    "No active transaction to roll back.".clone_into(&mut self.output);
                    return true;
                };
                self.confirm(
                    RequestKind::Transaction,
                    "explicit transaction rollback",
                    ProductOperation::TransactionRollback { handle },
                );
            }
            KeyCode::Backspace => {
                self.transaction_sql.pop();
            }
            KeyCode::Char(character) => {
                push_bounded(&mut self.transaction_sql, character, MAX_SQL_INPUT_BYTES);
            }
            _ => {}
        }
        true
    }

    fn handle_proofs_key(&mut self, code: KeyCode, controller: &mut OperationController) -> bool {
        if self.view() != View::Proofs {
            return false;
        }
        match code {
            KeyCode::F(5) => {
                let _accepted = self.submit(
                    controller,
                    RequestKind::ProofGenerate,
                    "bounded catalog proof",
                    ProductOperation::Prove {
                        operation: Box::new(ProductOperation::CatalogList(CatalogListRequest {
                            parent: None,
                            kind: None,
                            cursor: None,
                            item_limit: CATALOG_PAGE_ROWS,
                            visit_limit: CATALOG_VISIT_LIMIT,
                            byte_limit: CATALOG_BYTE_LIMIT,
                        })),
                        limits: console_proof_limits(),
                    },
                );
            }
            KeyCode::F(6) => {
                let Some(artifact) = &self.proof_artifact else {
                    "Generate a proof before verifying it.".clone_into(&mut self.output);
                    return true;
                };
                let _accepted = self.submit(
                    controller,
                    RequestKind::ProofVerify,
                    "offline proof verification",
                    ProductOperation::VerifyProof {
                        proof: artifact.proof.clone(),
                        witness: artifact.witness.clone(),
                        trusted_anchor: artifact.anchor,
                    },
                );
            }
            _ => {}
        }
        true
    }

    fn complete(&mut self, completed: CompletedOperation) {
        self.active_operation = None;
        let response = match completed.result {
            Ok(response) => response,
            Err(error) => {
                self.output = format!("{}: {error}", error.code().as_str());
                if matches!(completed.active.kind, RequestKind::Security(_)) {
                    self.security.panel = SecurityPanel::Message(self.output.clone());
                }
                if self.shutdown_pending {
                    self.should_quit = true;
                }
                return;
            }
        };
        match (completed.active.kind, &response) {
            (RequestKind::Overview, ProductResponse::Capabilities(capabilities)) => {
                self.capabilities = *capabilities;
            }
            (RequestKind::Catalog, ProductResponse::CatalogPage(page)) => {
                self.catalog_cursor = page.cursor;
            }
            (RequestKind::Catalog, ProductResponse::CatalogVisiblePage(page)) => {
                self.catalog_visible_cursor.clone_from(&page.cursor);
            }
            (
                RequestKind::Transaction,
                ProductResponse::ExplicitTransactionStatus(
                    ProductExplicitTransactionStatus::Active { handle, .. },
                ),
            ) => self.transaction_handle = Some(*handle),
            (
                RequestKind::Transaction,
                ProductResponse::TransactionCommitted(_)
                | ProductResponse::TransactionRolledBack(_),
            ) => self.transaction_handle = None,
            (RequestKind::ProofGenerate, ProductResponse::Proven { artifact, .. }) => {
                self.proof_artifact = Some(ProofArtifact {
                    proof: artifact.proof_bytes.clone(),
                    witness: artifact.witness_bytes.clone(),
                    anchor: artifact.trusted_anchor.digest(),
                });
            }
            (RequestKind::Security(section), _) => {
                self.security.panel = panel_from_response(section, response.clone());
            }
            _ => {}
        }
        self.output = render_response(response);
        if self.shutdown_pending {
            self.should_quit = true;
        }
    }
}

fn active_structure_input(app: &mut App) -> &mut String {
    if app.structure_field == 0 {
        &mut app.structure_key
    } else {
        &mut app.structure_value
    }
}

fn active_search_input(app: &mut App) -> &mut String {
    if app.search_field == 0 {
        &mut app.search_index
    } else {
        &mut app.search_query
    }
}

fn active_backup_input(app: &mut App) -> &mut String {
    if app.backup_field == 0 {
        &mut app.backup_source
    } else {
        &mut app.backup_destination
    }
}

fn parse_object_id(input: &str) -> Option<ObjectId> {
    input
        .trim()
        .parse::<u128>()
        .ok()
        .and_then(|id| ObjectId::new(id).ok())
}

fn push_bounded(input: &mut String, character: char, maximum: usize) {
    if !character.is_control() && input.len() + character.len_utf8() <= maximum {
        input.push(character);
    }
}

fn console_proof_limits() -> NativeProofGenerationLimits {
    let mut limits = NativeProofGenerationLimits::default();
    limits.admitted.result_items = 256;
    limits.admitted.candidate_items = 4_096;
    limits.admitted.evidence_bytes = 1024 * 1024;
    limits.proof.max_proof_bytes = 2 * 1024 * 1024;
    limits.proof.max_section_bytes = 1024 * 1024;
    limits.proof.max_decoded_bytes = 2 * 1024 * 1024;
    limits.proof.max_objects = 256;
    limits.witness.max_witness_bytes = 4 * 1024 * 1024;
    limits.witness.max_entries = 4_096;
    limits.witness.max_files = 2_048;
    limits.witness.max_directories = 2_048;
    limits.witness.max_file_bytes = 4 * 1024 * 1024;
    limits.witness.max_total_file_bytes = 4 * 1024 * 1024;
    limits.witness.max_decoded_bytes = 4 * 1024 * 1024;
    limits
}

fn render_response(response: ProductResponse) -> String {
    if let ProductResponse::CatalogVisiblePage(page) = response {
        return bounded_output(
            serde_json::to_string_pretty(&serde_json::json!({
                "items": page.items.into_iter().map(|item| serde_json::json!({
                    "id": item.id.get().to_string(),
                    "kind": format!("{:?}", item.kind).to_lowercase(),
                    "name": item.name.to_string(),
                    "parent": item.parent.map(|parent| parent.get().to_string()),
                })).collect::<Vec<_>>(),
                "continuation": if page.cursor.is_some() { "opaque cursor available" } else { "none" },
            }))
            .unwrap_or_else(|_| "unable to render response".to_owned()),
        );
    }
    bounded_output(
        serde_json::to_string_pretty(&super::response_json(response))
            .unwrap_or_else(|_| "unable to render response".to_owned()),
    )
}

/// Runs the interactive console while holding exclusive native ownership.
pub(crate) fn run(data_dir: PathBuf, mut client: EmbeddedClient) -> Result<(), CliFailure> {
    let mut app = App::new(data_dir, &mut client)?;
    let mut controller = OperationController::new(client);
    enable_raw_mode()?;
    let _guard = TerminalModeGuard;
    execute!(stdout(), EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    while !app.should_quit {
        if let Some(completed) = controller.poll() {
            app.complete(completed);
        }
        terminal.draw(|frame| render(frame, &app))?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            app.handle_key(key, &mut controller);
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
    match app.view() {
        View::Sql => render_sql(frame, app, area),
        View::Structures => render_structures(frame, app, area),
        View::Search => render_search(frame, app, area),
        View::Catalog => render_catalog(frame, app, area),
        View::Backups => render_backups(frame, app, area),
        View::Operations => render_operations(frame, app, area),
        View::Proofs => render_proofs(frame, app, area),
        View::Security => render_security(frame, app, area),
        View::Overview => frame.render_widget(
            Paragraph::new(overview_lines(app))
                .block(Block::default().borders(Borders::ALL).title("Overview"))
                .wrap(Wrap { trim: false }),
            area,
        ),
    }
}

fn render_workspace(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &'static str,
    input: Vec<Line<'static>>,
    app: &App,
) {
    let direction = if area.width < 100 {
        Direction::Vertical
    } else {
        Direction::Horizontal
    };
    let panes = Layout::default()
        .direction(direction)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);
    frame.render_widget(
        Paragraph::new(input)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false }),
        panes[0],
    );
    frame.render_widget(
        Paragraph::new(app.output.as_str())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Bounded result "),
            )
            .wrap(Wrap { trim: false }),
        panes[1],
    );
}

fn input_line(label: &'static str, value: &str, selected: bool) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            if selected { "> " } else { "  " },
            Style::default().fg(Color::LightCyan),
        ),
        Span::styled(
            format!("{label:>12}  "),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw(value.to_owned()),
    ])
}

fn render_structures(frame: &mut Frame<'_>, app: &App, area: Rect) {
    render_workspace(
        frame,
        area,
        " Structures / exact key only ",
        vec![
            input_line("key", &app.structure_key, app.structure_field == 0),
            input_line("value", &app.structure_value, app.structure_field == 1),
            Line::from(""),
            Line::from("Up/Down fields · Enter/F5 get · F6 TTL · F7 set"),
            Line::from("No key listing exists in ProductOperation."),
            Line::from(format!(
                "input bound: {MAX_FIELD_INPUT_BYTES} bytes per field"
            )),
        ],
        app,
    );
}

fn render_search(frame: &mut Frame<'_>, app: &App, area: Rect) {
    render_workspace(
        frame,
        area,
        " Search / bounded lexical term ",
        vec![
            input_line("index ID", &app.search_index, app.search_field == 0),
            input_line("query", &app.search_query, app.search_field == 1),
            Line::from(""),
            Line::from(format!("Enter executes · max {SEARCH_RESULT_LIMIT} hits")),
            Line::from(format!(
                "cooperative deadline: {CONSOLE_QUERY_TIMEOUT_SECONDS} seconds"
            )),
            Line::from(format!("query bound: {MAX_SEARCH_QUERY_BYTES} bytes")),
            Line::from("Vector and integrated requests remain available through typed CLI input."),
        ],
        app,
    );
}

fn render_catalog(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let continuation = if app.managed {
        if app.catalog_visible_cursor.is_some() {
            "opaque cursor available"
        } else {
            "none"
        }
    } else if app.catalog_cursor.is_some() {
        "snapshot cursor available"
    } else {
        "none"
    };
    render_workspace(
        frame,
        area,
        " Catalog / immutable bounded page ",
        vec![
            metric_line("Page items", CATALOG_PAGE_ROWS.to_string()),
            metric_line("Visit limit", CATALOG_VISIT_LIMIT.to_string()),
            metric_line("Byte limit", CATALOG_BYTE_LIMIT.to_string()),
            metric_line("Continuation", continuation.to_owned()),
            Line::from(""),
            Line::from("F5 first page · F6 next emitted cursor"),
            Line::from(if app.managed {
                "Listing is read-only, scope-visible, and continuation-opaque."
            } else {
                "Listing is read-only and snapshot-bound."
            }),
        ],
        app,
    );
}

fn render_backups(frame: &mut Frame<'_>, app: &App, area: Rect) {
    render_workspace(
        frame,
        area,
        " Backups / new destinations only ",
        vec![
            input_line("source", &app.backup_source, app.backup_field == 0),
            input_line(
                "destination",
                &app.backup_destination,
                app.backup_field == 1,
            ),
            Line::from(""),
            Line::from("F5 create backup · F6 restore · both require y confirmation"),
            Line::from("Create already verifies the promoted backup."),
            Line::from("No standalone backup.verify ProductOperation is invented."),
        ],
        app,
    );
}

fn render_operations(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let transaction = app
        .transaction_handle
        .map_or_else(|| "none".to_owned(), |handle| handle.get().to_string());
    render_workspace(
        frame,
        area,
        " Operations / status, telemetry, transaction ",
        vec![
            metric_line("Active tx", transaction),
            input_line("stage SQL", &app.transaction_sql, true),
            Line::from(""),
            Line::from("F5 status · F6 telemetry · F7 checkpoint"),
            Line::from("F8 begin · F9 stage SQL · F10 commit · F11 rollback"),
            Line::from("Checkpoint and every transaction action require confirmation."),
            Line::from("Compact/vacuum are absent: no ProductOperation exists."),
        ],
        app,
    );
}

fn render_proofs(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let artifact = app.proof_artifact.as_ref().map_or_else(
        || "none".to_owned(),
        |artifact| {
            format!(
                "{} proof bytes / {} witness bytes",
                artifact.proof.len(),
                artifact.witness.len()
            )
        },
    );
    render_workspace(
        frame,
        area,
        " Proofs / bounded in-memory artifact ",
        vec![
            metric_line("Retained", artifact),
            Line::from(""),
            Line::from("F5 prove the first bounded catalog page"),
            Line::from("F6 verify the retained proof with its trusted anchor"),
            Line::from("Only read-only ProductOperation input is proven."),
        ],
        app,
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
    let help = if app.confirmation.is_some() {
        "y confirm  n/Esc cancel"
    } else if app.active_operation.is_some() {
        "one active operation  Esc cancel  queue capacity 1"
    } else {
        match app.view() {
            View::Sql => "Tab/Left/Right views  Enter run  Esc exit",
            View::Structures => "Up/Down fields  Enter/F5 get  F6 TTL  F7 set  Esc exit",
            View::Search => "Up/Down fields  Enter search  Esc exit",
            View::Catalog => "F5 first  F6 next  Tab views  q/Esc exit",
            View::Backups => "Up/Down fields  F5 backup  F6 restore  Esc exit",
            View::Operations => "F5-F11 actions  type stage SQL  Esc exit",
            View::Proofs => "F5 generate  F6 verify  q/Esc exit",
            View::Security => "Up/Down security views  n next  r first  q/Esc exit",
            View::Overview => "Tab/Left/Right views  r refresh  q/Esc exit",
        }
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
        Line::from("One non-blocking operation; bounded channel capacity one; Esc cancels."),
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
            structure_key: "key".to_owned(),
            structure_value: "value".to_owned(),
            structure_field: 0,
            search_index: "17".to_owned(),
            search_query: "hyphae".to_owned(),
            search_field: 1,
            catalog_cursor: None,
            catalog_visible_cursor: None,
            backup_source: "/var/backups/hyphae".to_owned(),
            backup_destination: "/var/backups/hyphae-next".to_owned(),
            backup_field: 1,
            transaction_sql: "UPDATE items SET value = 'bounded' WHERE id = 1".to_owned(),
            transaction_handle: None,
            proof_artifact: None,
            confirmation: None,
            active_operation: None,
            output: "ready".to_owned(),
            shutdown_pending: false,
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

    fn enter_security(app: &mut App, controller: &mut OperationController) {
        for _ in 0..8 {
            app.handle_key(press(KeyCode::Tab), controller);
        }
    }

    fn wait_for_completion(
        app: &mut App,
        controller: &mut OperationController,
    ) -> Result<(), Box<dyn Error>> {
        for _ in 0..1_000 {
            if let Some(completed) = controller.poll() {
                app.complete(completed);
                return Ok(());
            }
            thread::sleep(Duration::from_millis(1));
        }
        Err("TUI operation did not complete".into())
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
        let mut controller = OperationController::new(client);
        enter_security(&mut app, &mut controller);
        assert_eq!(app.view(), View::Security);
        app.handle_key(press(KeyCode::Down), &mut controller);
        wait_for_completion(&mut app, &mut controller)?;
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

        app.handle_key(press(KeyCode::Char('n')), &mut controller);
        wait_for_completion(&mut app, &mut controller)?;
        let second_id = match &app.security.panel {
            SecurityPanel::Principals(page) => page
                .items
                .first()
                .ok_or("empty second principal page")?
                .id(),
            _ => return Err("security n did not load the continuation".into()),
        };
        assert_ne!(first_id, second_id);

        app.handle_key(press(KeyCode::Char('r')), &mut controller);
        wait_for_completion(&mut app, &mut controller)?;
        let refreshed_id = match &app.security.panel {
            SecurityPanel::Principals(page) => page
                .items
                .first()
                .ok_or("empty refreshed principal page")?
                .id(),
            _ => return Err("security r did not reload the first page".into()),
        };
        assert_eq!(refreshed_id, first_id);
        app.handle_key(press(KeyCode::Up), &mut controller);
        wait_for_completion(&mut app, &mut controller)?;
        assert_eq!(app.security.section(), SecuritySection::Status);
        assert!(matches!(app.security.panel, SecurityPanel::Status(_)));
        Ok(())
    }

    #[test]
    fn managed_denial_stays_typed_inside_the_security_panel() -> Result<(), Box<dyn Error>> {
        let fixture = ManagedTuiFixture::create()?;
        let mut client = fixture.reader_client()?;
        let mut app = App::new(fixture.data.clone(), &mut client)?;
        let mut controller = OperationController::new(client);
        enter_security(&mut app, &mut controller);
        app.handle_key(press(KeyCode::Down), &mut controller);
        wait_for_completion(&mut app, &mut controller)?;
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
        let mut controller = OperationController::new(client);
        enter_security(&mut app, &mut controller);
        app.handle_key(press(KeyCode::Char('r')), &mut controller);
        wait_for_completion(&mut app, &mut controller)?;
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
                app.handle_key(press(KeyCode::Down), &mut controller);
                wait_for_completion(&mut app, &mut controller)?;
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
    fn bounded_workspaces_replace_placeholders_at_compact_and_wide_sizes() -> Result<(), Infallible>
    {
        let cases = [
            (View::Structures, "No key listing exists"),
            (View::Search, "max 20 hits"),
            (View::Catalog, "Visit limit"),
            (View::Backups, "No standalone backup.verify"),
            (View::Operations, "F5 status"),
            (View::Proofs, "Only read-only ProductOperation"),
        ];
        for (view, marker) in cases {
            for (width, height) in [(80, 24), (120, 36), (200, 60)] {
                let rendered = fixture(width, height, view)?;
                assert!(rendered.contains(marker), "missing {marker} in {view:?}");
                assert!(!rendered.contains("interactive actions land next"));
            }
        }
        Ok(())
    }

    #[test]
    fn controller_exposes_capacity_one_backpressure_and_cooperative_cancel()
    -> Result<(), Box<dyn Error>> {
        let fixture = ManagedTuiFixture::create()?;
        let mut controller = OperationController::new(fixture.owner_client()?);
        controller.submit(
            RequestKind::Operations,
            "first",
            ProductOperation::AdminStatus,
        )?;
        let Err(rejected) = controller.submit(
            RequestKind::Operations,
            "second",
            ProductOperation::Telemetry,
        ) else {
            return Err("second active operation was admitted".into());
        };
        assert!(rejected.contains("Backpressure"));
        assert!(controller.cancel());
        wait_for_controller(&mut controller)?;
        Ok(())
    }

    #[test]
    fn tui_exits_after_deterministic_mid_execution_cancellation_without_hang()
    -> Result<(), Box<dyn Error>> {
        let fixture = ManagedTuiFixture::create()?;
        let mut client = fixture.owner_client()?;
        let mut app = App::new(fixture.data.clone(), &mut client)?;
        let mut controller = OperationController::with_dispatch(|task| {
            while !task.cancellation.is_cancelled() {
                thread::yield_now();
            }
            Err(Box::new(hyphae_native_product::ProductError::from_code(
                hyphae_native_product::ProductErrorCode::Cancelled,
            )))
        });
        assert!(app.submit(
            &mut controller,
            RequestKind::Sql,
            "cancelled SQL",
            ProductOperation::ExecuteSql {
                statement: "SELECT id FROM rows ORDER BY id LIMIT 256".to_owned(),
                parameters: Vec::new(),
            },
        ));
        app.handle_key(press(KeyCode::Esc), &mut controller);
        assert!(app.shutdown_pending);
        wait_for_completion(&mut app, &mut controller)?;
        assert!(app.should_quit);
        assert!(app.output.contains("cancelled"));
        Ok(())
    }

    #[test]
    fn exit_is_refused_until_an_active_operation_completes() -> Result<(), Box<dyn Error>> {
        let fixture = ManagedTuiFixture::create()?;
        let mut client = fixture.owner_client()?;
        let mut app = App::new(fixture.data.clone(), &mut client)?;
        let mut controller = OperationController::new(client);
        controller.submit(
            RequestKind::Operations,
            "active",
            ProductOperation::AdminStatus,
        )?;
        app.handle_key(press(KeyCode::Esc), &mut controller);
        assert!(!app.should_quit);
        assert!(app.shutdown_pending);
        assert!(app.output.contains("exit is refused"));
        wait_for_completion(&mut app, &mut controller)?;
        assert!(app.should_quit);
        Ok(())
    }

    #[test]
    fn catalog_refresh_preserves_cursor_when_backpressure_rejects_submit()
    -> Result<(), Box<dyn Error>> {
        let fixture = ManagedTuiFixture::create()?;
        let mut client = fixture.owner_client()?;
        let mut app = App::new(fixture.data.clone(), &mut client)?;
        let mut controller = OperationController::new(client);
        app.view_index = View::ALL
            .iter()
            .position(|view| *view == View::Catalog)
            .ok_or("missing catalog view")?;
        let cursor = CatalogVisibleCursor::new(b"opaque-cursor".to_vec())?;
        app.catalog_visible_cursor = Some(cursor.clone());
        controller.submit(
            RequestKind::Operations,
            "occupied",
            ProductOperation::AdminStatus,
        )?;
        app.handle_key(press(KeyCode::F(5)), &mut controller);
        assert_eq!(app.catalog_visible_cursor, Some(cursor));
        assert!(app.output.contains("Backpressure"));
        wait_for_controller(&mut controller)?;
        Ok(())
    }

    #[test]
    fn rejected_security_navigation_keeps_section_and_panel_associated()
    -> Result<(), Box<dyn Error>> {
        let fixture = ManagedTuiFixture::create()?;
        let mut client = fixture.owner_client()?;
        let mut app = App::new(fixture.data.clone(), &mut client)?;
        let mut controller = OperationController::new(client);
        enter_security(&mut app, &mut controller);
        controller.submit(
            RequestKind::Overview,
            "occupied",
            ProductOperation::Capabilities,
        )?;
        let previous = app.security.section();
        app.handle_key(press(KeyCode::Down), &mut controller);
        assert_eq!(app.security.section(), previous);
        assert!(matches!(app.security.panel, SecurityPanel::Message(_)));
        wait_for_controller(&mut controller)?;
        Ok(())
    }

    #[test]
    fn catalog_visible_response_formatter_never_exposes_opaque_cursor() -> Result<(), Box<dyn Error>>
    {
        let cursor = CatalogVisibleCursor::new(b"opaque-cursor-canary".to_vec())?;
        let rendered = render_response(ProductResponse::CatalogVisiblePage(
            hyphae_native_product::CatalogVisiblePage {
                items: Vec::new(),
                cursor: Some(cursor),
            },
        ));
        assert!(rendered.contains("opaque cursor available"));
        assert!(!rendered.contains("opaque-cursor-canary"));
        Ok(())
    }

    #[test]
    fn destructive_actions_require_confirmation_and_input_is_bounded() -> Result<(), Box<dyn Error>>
    {
        let fixture = ManagedTuiFixture::create()?;
        let mut client = fixture.owner_client()?;
        let mut app = App::new(fixture.data.clone(), &mut client)?;
        let mut controller = OperationController::new(client);
        app.view_index = View::ALL
            .iter()
            .position(|view| *view == View::Structures)
            .ok_or("missing structures view")?;
        app.structure_key = "key".to_owned();
        app.structure_value = "value".to_owned();
        app.handle_key(press(KeyCode::F(7)), &mut controller);
        assert!(app.confirmation.is_some());
        assert!(controller.active_label().is_none());
        app.handle_key(press(KeyCode::Char('n')), &mut controller);
        assert!(app.confirmation.is_none());
        assert!(controller.active_label().is_none());

        let mut input = "a".repeat(MAX_SEARCH_QUERY_BYTES);
        push_bounded(&mut input, 'b', MAX_SEARCH_QUERY_BYTES);
        assert_eq!(input.len(), MAX_SEARCH_QUERY_BYTES);
        let mut utf8 = "a".repeat(MAX_SEARCH_QUERY_BYTES - 1);
        push_bounded(&mut utf8, 'á', MAX_SEARCH_QUERY_BYTES);
        assert_eq!(utf8.len(), MAX_SEARCH_QUERY_BYTES - 1);
        Ok(())
    }

    #[test]
    fn workspace_operations_are_only_existing_product_operations() -> Result<(), Box<dyn Error>> {
        let index = ObjectId::new(7)?;
        let operations = [
            ProductOperation::StructureGet { key: b"k".to_vec() },
            ProductOperation::StructureTtl { key: b"k".to_vec() },
            ProductOperation::Search {
                index,
                query: BoundedSearchQuery::Term("term".to_owned()),
                limit: SEARCH_RESULT_LIMIT,
            },
            ProductOperation::CatalogList(CatalogListRequest {
                parent: None,
                kind: None,
                cursor: None,
                item_limit: CATALOG_PAGE_ROWS,
                visit_limit: CATALOG_VISIT_LIMIT,
                byte_limit: CATALOG_BYTE_LIMIT,
            }),
            ProductOperation::CatalogVisibleList(CatalogVisibleListRequest {
                filter: CatalogVisibleListFilter {
                    parent: None,
                    kind: None,
                },
                cursor: None,
                item_limit: CATALOG_PAGE_ROWS,
                visit_limit: CATALOG_VISIT_LIMIT,
                byte_limit: CATALOG_BYTE_LIMIT,
            }),
            ProductOperation::AdminStatus,
            ProductOperation::Telemetry,
            ProductOperation::AdminCheckpoint,
        ];
        assert!(matches!(
            operations[0],
            ProductOperation::StructureGet { .. }
        ));
        assert!(matches!(
            operations[2],
            ProductOperation::Search { limit: 20, .. }
        ));
        let ProductOperation::CatalogList(request) = operations[3] else {
            return Err("catalog operation changed".into());
        };
        assert_eq!(request.item_limit, CATALOG_PAGE_ROWS);
        assert_eq!(request.visit_limit, CATALOG_VISIT_LIMIT);
        assert_eq!(request.byte_limit, CATALOG_BYTE_LIMIT);
        let ProductOperation::CatalogVisibleList(request) = &operations[4] else {
            return Err("visible catalog operation changed".into());
        };
        assert_eq!(request.item_limit, CATALOG_PAGE_ROWS);
        assert_eq!(request.visit_limit, CATALOG_VISIT_LIMIT);
        assert_eq!(request.byte_limit, CATALOG_BYTE_LIMIT);
        assert!(matches!(operations[7], ProductOperation::AdminCheckpoint));
        Ok(())
    }

    #[test]
    fn output_bound_preserves_utf8_boundary() {
        let output = "á".repeat(MAX_OUTPUT_BYTES);
        let bounded = bounded_output(output);
        assert!(bounded.is_char_boundary(bounded.len()));
        assert!(bounded.ends_with("output truncated by console bound"));
    }

    fn wait_for_controller(controller: &mut OperationController) -> Result<(), Box<dyn Error>> {
        wait_for_completed_operation(controller).map(|_| ())
    }

    fn wait_for_completed_operation(
        controller: &mut OperationController,
    ) -> Result<CompletedOperation, Box<dyn Error>> {
        for _ in 0..1_000 {
            if let Some(completed) = controller.poll() {
                return Ok(completed);
            }
            thread::sleep(Duration::from_millis(1));
        }
        Err("TUI controller did not finish".into())
    }
}
