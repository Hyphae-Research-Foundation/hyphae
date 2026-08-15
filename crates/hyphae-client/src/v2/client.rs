// SPDX-License-Identifier: AGPL-3.0-only

use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use hyphae_native_product::{
    BackupRequest, BoundedSearchQuery, CatalogDependencyRequest, CatalogListRequest, DoctorRequest,
    ObjectId, ProductDurabilityPolicy, ProductError, ProductLimits, ProductOperation,
    ProductPreparedHandle, ProductResponse, ProductSearchDocumentDelete,
    ProductSearchDocumentUpdate, ProductSearchIngestBatch, ProductSearchRequest,
    ProductStructureMutation, ProductStructureReadRequest, ProductValue, RestoreRequest,
    SecurityAssignmentListRequest, SecurityAuditReadRequest, SecurityKeyListRequest,
    SecurityPrincipalListRequest, SecurityRoleListRequest,
};

use super::{HttpTransport, LocalTransport};

/// Boxed request completion returned by a v2 transport.
pub type ResponseFuture<'request> =
    Pin<Box<dyn Future<Output = Result<ProductResponse, ClientError>> + Send + 'request>>;

/// Per-request deadline, cancellation, resource, durability, and identity controls.
#[derive(Clone, Debug)]
pub struct RequestOptions {
    /// Stable nonzero product request identity, generated when omitted.
    pub request_id: Option<u64>,
    /// Stable nonzero idempotency token for mutation retries.
    pub idempotency_token: Option<u128>,
    /// Logical time used by snapshots and TTL behavior.
    pub logical_time_micros: i64,
    /// Positive absolute Unix-time deadline in microseconds.
    pub deadline_micros: Option<i64>,
    /// Central resource envelope.
    pub limits: ProductLimits,
    /// Mutation acknowledgement policy.
    pub durability: ProductDurabilityPolicy,
    /// Cooperative cancellation observed by local and HTTP transports.
    pub cancellation: CancellationToken,
}

impl Default for RequestOptions {
    fn default() -> Self {
        Self {
            request_id: None,
            idempotency_token: None,
            logical_time_micros: 0,
            deadline_micros: None,
            limits: ProductLimits::default(),
            durability: ProductDurabilityPolicy::STRICT,
            cancellation: CancellationToken::new(),
        }
    }
}

/// Cloneable cooperative cancellation handle.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    state: Arc<CancellationState>,
}

#[derive(Debug, Default)]
struct CancellationState {
    cancelled: AtomicBool,
    notification: tokio::sync::Notify,
}

impl CancellationToken {
    /// Creates an uncancelled token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation. Repeating this call is harmless.
    pub fn cancel(&self) {
        if !self.state.cancelled.swap(true, Ordering::AcqRel) {
            self.state.notification.notify_waiters();
        }
    }

    /// Returns whether cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    pub(crate) async fn cancelled(&self) {
        let notified = self.state.notification.notified();
        tokio::pin!(notified);
        // Registration must precede the state read or cancellation between
        // those operations could be lost by `Notify::notify_waiters`.
        let _notification_ready = notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

/// Transport, protocol, contract, or stable product failure.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The product rejected the operation with typed stable fields.
    #[error(transparent)]
    Product(#[from] Box<ProductError>),
    /// Local transport failed before a definitive response completed.
    #[error("Hyphae native-local transport failed: {0}")]
    Local(String),
    /// HTTP transport failed before a definitive response completed.
    #[error("Hyphae HTTP v2 transport failed: {0}")]
    Http(String),
    /// A binary payload violated the native product contract.
    #[error("Hyphae v2 protocol payload is invalid: {0}")]
    Protocol(String),
    /// The operation was cancelled before a definitive response.
    #[error("Hyphae v2 operation was cancelled")]
    Cancelled,
    /// A transport returned another response variant.
    #[error("Hyphae v2 transport returned an unexpected response")]
    UnexpectedResponse,
}

/// Operation transport implemented by native-local and HTTP `/v2` clients.
pub trait Transport: Send + Sync {
    /// Executes one transport-independent product operation.
    fn execute(&self, operation: ProductOperation, options: RequestOptions) -> ResponseFuture<'_>;
}

/// Equivalent high-level Native v2 client independent of the selected transport.
#[derive(Clone)]
pub struct HyphaeClient {
    transport: Arc<dyn Transport>,
}

impl std::fmt::Debug for HyphaeClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HyphaeClient")
            .finish_non_exhaustive()
    }
}

#[allow(clippy::missing_errors_doc)]
impl HyphaeClient {
    /// All high-level operations return [`ClientError`] when transport,
    /// protocol, cancellation, or typed product execution fails.
    ///
    /// # Errors
    ///
    /// See the individual transport and product operation contracts.
    /// Wraps any conforming v2 transport.
    pub fn new(transport: impl Transport + 'static) -> Self {
        Self {
            transport: Arc::new(transport),
        }
    }

    /// Creates an HTTP `/v2` client.
    pub fn http(base_url: &str) -> Result<Self, ClientError> {
        Ok(Self::new(HttpTransport::new(base_url)?))
    }

    /// Creates an exact `HYPHLCL1` UDS or Windows named-pipe client.
    pub fn local(endpoint: impl Into<String>) -> Result<Self, ClientError> {
        Ok(Self::new(LocalTransport::new(endpoint)?))
    }

    /// Creates an API-key-authenticated local client.
    pub fn local_authenticated(
        endpoint: impl Into<String>,
        api_key: impl AsRef<str>,
    ) -> Result<Self, ClientError> {
        Ok(Self::new(LocalTransport::new(endpoint)?.api_key(api_key)?))
    }

    /// Creates a local client with an explicit bounded handshake identity.
    pub fn local_with_identity(
        endpoint: impl Into<String>,
        client_identity: impl Into<String>,
    ) -> Result<Self, ClientError> {
        Ok(Self::new(
            LocalTransport::new(endpoint)?.client_identity(client_identity)?,
        ))
    }

    /// Executes one transport-independent product operation.
    pub async fn execute(
        &self,
        operation: ProductOperation,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.transport.execute(operation, options).await
    }

    /// Discovers product capabilities.
    pub async fn capabilities(
        &self,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(ProductOperation::Capabilities, options).await
    }

    /// Lists a bounded catalog page.
    pub async fn catalog_list(
        &self,
        request: CatalogListRequest,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(ProductOperation::CatalogList(request), options)
            .await
    }

    /// Lists a bounded dependency page.
    pub async fn catalog_dependencies(
        &self,
        request: CatalogDependencyRequest,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(ProductOperation::CatalogDependencies(request), options)
            .await
    }

    /// Executes direct SQL with canonical typed parameters.
    pub async fn sql(
        &self,
        statement: impl Into<String>,
        parameters: Vec<ProductValue>,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(
            ProductOperation::ExecuteSql {
                statement: statement.into(),
                parameters,
            },
            options,
        )
        .await
    }

    /// Prepares one session-local SQL statement.
    pub async fn prepare_sql(
        &self,
        statement: impl Into<String>,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(
            ProductOperation::PrepareSql {
                statement: statement.into(),
            },
            options,
        )
        .await
    }

    /// Executes one session-local prepared SQL handle.
    pub async fn execute_prepared(
        &self,
        handle: ProductPreparedHandle,
        parameters: Vec<ProductValue>,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(
            ProductOperation::ExecutePrepared { handle, parameters },
            options,
        )
        .await
    }

    /// Reads one scalar structure value.
    pub async fn structure_get(
        &self,
        key: Vec<u8>,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(ProductOperation::StructureGet { key }, options)
            .await
    }

    /// Sets one scalar structure value and optional absolute expiry.
    pub async fn structure_set(
        &self,
        key: Vec<u8>,
        value: Vec<u8>,
        expires_at_micros: Option<i64>,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(
            ProductOperation::StructureSet {
                key,
                value,
                expires_at_micros,
            },
            options,
        )
        .await
    }

    /// Reads one scalar structure TTL.
    pub async fn structure_ttl(
        &self,
        key: Vec<u8>,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(ProductOperation::StructureTtl { key }, options)
            .await
    }

    /// Applies a nonempty atomic mutation batch across all structure families.
    pub async fn structure_mutate(
        &self,
        mutations: Vec<ProductStructureMutation>,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(ProductOperation::StructureMutate { mutations }, options)
            .await
    }

    /// Reads one non-scalar native structure family.
    pub async fn structure_read(
        &self,
        request: ProductStructureReadRequest,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(ProductOperation::StructureRead(request), options)
            .await
    }

    /// Executes bounded native lexical search.
    pub async fn search(
        &self,
        index: ObjectId,
        query: BoundedSearchQuery,
        limit: usize,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(
            ProductOperation::Search {
                index,
                query,
                limit,
            },
            options,
        )
        .await
    }

    /// Executes integrated lexical, named-vector, ANN, or hybrid search.
    pub async fn search_collection(
        &self,
        collection: ObjectId,
        request: ProductSearchRequest,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(
            ProductOperation::SearchCollection {
                collection,
                request,
            },
            options,
        )
        .await
    }

    /// Atomically ingests integrated documents across all collection branches.
    pub async fn search_ingest(
        &self,
        collection: ObjectId,
        batch: ProductSearchIngestBatch,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(
            ProductOperation::SearchIngest { collection, batch },
            options,
        )
        .await
    }

    /// Replaces one integrated document across all collection branches.
    pub async fn search_document_update(
        &self,
        collection: ObjectId,
        update: ProductSearchDocumentUpdate,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(
            ProductOperation::SearchDocumentUpdate { collection, update },
            options,
        )
        .await
    }

    /// Deletes one integrated document from all collection branches.
    pub async fn search_document_delete(
        &self,
        collection: ObjectId,
        delete: ProductSearchDocumentDelete,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(
            ProductOperation::SearchDocumentDelete { collection, delete },
            options,
        )
        .await
    }

    /// Captures current administrative status.
    pub async fn admin_status(
        &self,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(ProductOperation::AdminStatus, options).await
    }

    /// Publishes a synchronized checkpoint.
    pub async fn checkpoint(
        &self,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(ProductOperation::AdminCheckpoint, options)
            .await
    }

    /// Captures the bounded process-local telemetry registry.
    pub async fn telemetry(&self, options: RequestOptions) -> Result<ProductResponse, ClientError> {
        self.execute(ProductOperation::Telemetry, options).await
    }

    /// Reads redacted access-control catalog status.
    pub async fn security_status(
        &self,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(ProductOperation::SecurityStatus, options)
            .await
    }

    /// Lists one bounded redacted principal page.
    pub async fn security_principal_list(
        &self,
        request: SecurityPrincipalListRequest,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(ProductOperation::SecurityPrincipalList(request), options)
            .await
    }

    /// Lists one bounded redacted role page.
    pub async fn security_role_list(
        &self,
        request: SecurityRoleListRequest,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(ProductOperation::SecurityRoleList(request), options)
            .await
    }

    /// Lists one bounded redacted assignment page.
    pub async fn security_assignment_list(
        &self,
        request: SecurityAssignmentListRequest,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(ProductOperation::SecurityAssignmentList(request), options)
            .await
    }

    /// Lists one bounded redacted API-key metadata page.
    pub async fn security_key_list(
        &self,
        request: SecurityKeyListRequest,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(ProductOperation::SecurityKeyList(request), options)
            .await
    }

    /// Reads one bounded redacted security-audit page.
    pub async fn security_audit_read(
        &self,
        request: SecurityAuditReadRequest,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(ProductOperation::SecurityAuditRead(request), options)
            .await
    }

    /// Explains one SQL statement.
    pub async fn explain_sql(
        &self,
        statement: impl Into<String>,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(
            ProductOperation::AdminExplainSql {
                statement: statement.into(),
            },
            options,
        )
        .await
    }

    /// Runs typed directory diagnosis.
    pub async fn doctor(
        &self,
        request: DoctorRequest,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(ProductOperation::Doctor(request), options)
            .await
    }

    /// Creates and verifies a native backup.
    pub async fn backup(
        &self,
        request: BackupRequest,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(ProductOperation::Backup(request), options)
            .await
    }

    /// Verifies and restores a backup to a separate new native directory.
    pub async fn restore(
        &self,
        request: RestoreRequest,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(ProductOperation::Restore(request), options)
            .await
    }

    /// Resolves retained transaction evidence after an uncertain outcome.
    pub async fn transaction_status(
        &self,
        transaction_id: hyphae_native_product::ProductTransactionId,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(
            ProductOperation::TransactionStatus { transaction_id },
            options,
        )
        .await
    }

    /// Resolves durable transaction evidence by caller idempotency token.
    pub async fn transaction_status_by_idempotency(
        &self,
        idempotency_token: u128,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(
            ProductOperation::TransactionStatusByIdempotency { idempotency_token },
            options,
        )
        .await
    }

    /// Verifies canonical proof and witness artifacts without the origin directory.
    pub async fn verify_proof(
        &self,
        proof: Vec<u8>,
        witness: Vec<u8>,
        trusted_anchor: [u8; 32],
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(
            ProductOperation::VerifyProof {
                proof,
                witness,
                trusted_anchor,
            },
            options,
        )
        .await
    }

    /// Executes one eligible read and returns its native proof and retained witness.
    pub async fn prove(
        &self,
        operation: ProductOperation,
        limits: hyphae_native_product::proof::NativeProofGenerationLimits,
        options: RequestOptions,
    ) -> Result<ProductResponse, ClientError> {
        self.execute(
            ProductOperation::Prove {
                operation: Box::new(operation),
                limits,
            },
            options,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        pin::Pin,
        sync::{
            Arc, Barrier,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll, Wake, Waker},
        thread,
    };

    use super::CancellationToken;

    #[derive(Default)]
    struct WakeCounter {
        wakes: AtomicUsize,
    }

    impl Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.wakes.fetch_add(1, Ordering::Relaxed);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.wakes.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn poll_once<F>(future: Pin<&mut F>, waker: &Waker) -> Poll<()>
    where
        F: Future<Output = ()>,
    {
        future.poll(&mut Context::from_waker(waker))
    }

    #[test]
    fn pre_cancelled_token_completes_on_first_poll() {
        let token = CancellationToken::new();
        token.cancel();
        let mut cancelled = Box::pin(token.cancelled());

        assert_eq!(
            poll_once(cancelled.as_mut(), Waker::noop()),
            Poll::Ready(())
        );
    }

    #[test]
    fn cancellation_wakes_every_registered_waiter() {
        let token = CancellationToken::new();
        let counters = (0..4)
            .map(|_| Arc::new(WakeCounter::default()))
            .collect::<Vec<_>>();
        let wakers = counters
            .iter()
            .cloned()
            .map(Waker::from)
            .collect::<Vec<_>>();
        let mut waiters = (0..counters.len())
            .map(|_| Box::pin(token.cancelled()))
            .collect::<Vec<_>>();

        for (waiter, waker) in waiters.iter_mut().zip(&wakers) {
            assert_eq!(poll_once(waiter.as_mut(), waker), Poll::Pending);
        }
        token.cancel();

        for ((waiter, waker), counter) in waiters.iter_mut().zip(&wakers).zip(&counters) {
            assert_eq!(counter.wakes.load(Ordering::Relaxed), 1);
            assert_eq!(poll_once(waiter.as_mut(), waker), Poll::Ready(()));
        }
    }

    #[test]
    fn cancellation_registration_races_do_not_lose_wakeup() {
        for _ in 0..128 {
            let token = CancellationToken::new();
            let waiter_token = token.clone();
            let start = Arc::new(Barrier::new(2));
            let first_poll_finished = Arc::new(Barrier::new(2));
            let cancellation_finished = Arc::new(Barrier::new(2));
            let waiter = thread::spawn({
                let start = start.clone();
                let first_poll_finished = first_poll_finished.clone();
                let cancellation_finished = cancellation_finished.clone();
                move || {
                    let mut cancelled = Box::pin(waiter_token.cancelled());
                    start.wait();
                    let first = poll_once(cancelled.as_mut(), Waker::noop());
                    first_poll_finished.wait();
                    cancellation_finished.wait();
                    if first.is_ready() {
                        first
                    } else {
                        poll_once(cancelled.as_mut(), Waker::noop())
                    }
                }
            });

            start.wait();
            token.cancel();
            first_poll_finished.wait();
            cancellation_finished.wait();

            assert!(matches!(waiter.join(), Ok(Poll::Ready(()))));
        }
    }
}
