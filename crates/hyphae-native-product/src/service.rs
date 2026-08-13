// SPDX-License-Identifier: AGPL-3.0-only

//! One-owner bounded multi-client product operation service.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::Instant,
};

use crate::{
    MetricId, NativeProduct, ProductAuthorization, ProductError, ProductErrorCode,
    ProductOperation, ProductPrincipal, ProductRequestContext, ProductResponse, ProductSession,
    ProductSessionId, TelemetryRegistry, TimingClass,
};

/// Default bounded product-service request queue.
pub const DEFAULT_PRODUCT_SERVICE_QUEUE_CAPACITY: usize = 256;
/// Default simultaneous product sessions.
pub const DEFAULT_PRODUCT_SERVICE_SESSIONS: usize = 1_024;

/// Service queue and session retention bounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeProductServiceConfig {
    /// Maximum requests waiting for the sole owner.
    pub queue_capacity: usize,
    /// Maximum retained client sessions.
    pub max_sessions: usize,
    /// Maximum retained prepared plans per session.
    pub max_prepared_per_session: usize,
    /// Maximum retained transaction outcomes per session.
    pub max_transaction_statuses_per_session: usize,
    /// Maximum simultaneous explicit all-engine transactions per session.
    pub max_active_transactions_per_session: usize,
}

impl Default for NativeProductServiceConfig {
    fn default() -> Self {
        Self {
            queue_capacity: DEFAULT_PRODUCT_SERVICE_QUEUE_CAPACITY,
            max_sessions: DEFAULT_PRODUCT_SERVICE_SESSIONS,
            max_prepared_per_session: crate::DEFAULT_PRODUCT_PREPARED_HANDLES,
            max_transaction_statuses_per_session: crate::DEFAULT_PRODUCT_TRANSACTION_STATUSES,
            max_active_transactions_per_session: crate::DEFAULT_PRODUCT_ACTIVE_TRANSACTIONS,
        }
    }
}

impl NativeProductServiceConfig {
    fn validate(self) -> Result<(), ProductError> {
        if self.queue_capacity == 0
            || self.max_sessions == 0
            || self.max_prepared_per_session == 0
            || self.max_transaction_statuses_per_session == 0
            || self.max_active_transactions_per_session == 0
        {
            Err(ProductError::from_code(ProductErrorCode::InvalidRequest))
        } else {
            Ok(())
        }
    }
}

enum ServiceCommand {
    OpenSession {
        session_id: ProductSessionId,
        principal: ProductPrincipal,
        authorization: ProductAuthorization,
        reply: SyncSender<Result<ProductSessionId, ProductError>>,
    },
    Dispatch {
        session_id: ProductSessionId,
        context: ProductRequestContext,
        operation: Box<ProductOperation>,
        enqueued_at: Instant,
        reply: DispatchReply,
    },
    CloseSession {
        session_id: ProductSessionId,
        reply: SyncSender<()>,
    },
    Shutdown {
        reply: SyncSender<()>,
    },
}

enum DispatchReply {
    Blocking(SyncSender<Result<ProductResponse, ProductError>>),
    Async(futures_channel::oneshot::Sender<Result<ProductResponse, ProductError>>),
}

impl DispatchReply {
    fn send(self, response: Result<ProductResponse, ProductError>) {
        match self {
            Self::Blocking(reply) => {
                let _ignored = reply.send(response);
            }
            Self::Async(reply) => {
                let _ignored = reply.send(response);
            }
        }
    }
}

struct SharedService {
    sender: SyncSender<ServiceCommand>,
    accepting: AtomicBool,
    next_session_id: Mutex<u128>,
    admission: Mutex<ServiceAdmission>,
    telemetry: TelemetryRegistry,
    queue_depth: AtomicU64,
    product: RwLock<Option<NativeProduct>>,
    sessions: Mutex<BTreeMap<ProductSessionId, Arc<RwLock<ProductSession>>>>,
    #[cfg(test)]
    fast_structure_get_hits: AtomicU64,
    #[cfg(test)]
    fast_structure_get_fallbacks: AtomicU64,
    #[cfg(test)]
    test_hooks: Mutex<ServiceTestHooks>,
}

#[derive(Default)]
struct ServiceAdmission {
    pending_owner_commands: u64,
}

impl ServiceAdmission {
    fn reserve_owner(&mut self) -> Result<(), ProductError> {
        self.pending_owner_commands = self
            .pending_owner_commands
            .checked_add(1)
            .ok_or_else(|| ProductError::from_code(ProductErrorCode::LimitExceeded))?;
        Ok(())
    }

    fn retire_owner(&mut self) -> Result<(), ProductError> {
        self.pending_owner_commands = self
            .pending_owner_commands
            .checked_sub(1)
            .ok_or_else(|| ProductError::from_code(ProductErrorCode::Internal))?;
        Ok(())
    }
}

#[cfg(test)]
#[derive(Default)]
struct ServiceTestHooks {
    dispatch_attempt: Option<SyncSender<()>>,
    fallback_enqueue_pause: Option<ServiceTestPause>,
    fast_execute_pause: Option<ServiceTestPause>,
    owner_dequeue_pause: Option<ServiceTestPause>,
}

#[cfg(test)]
struct ServiceTestPause {
    entered: SyncSender<()>,
    release: Receiver<()>,
}

#[cfg(test)]
impl ServiceTestPause {
    fn wait(self) {
        let _ignored = self.entered.send(());
        let _ignored = self.release.recv();
    }
}

/// Cloneable ingress handle for one bounded native product owner.
#[derive(Clone)]
pub struct NativeProductHandle {
    shared: Arc<SharedService>,
}

impl NativeProductHandle {
    /// Opens one bounded service-owned session.
    ///
    /// # Errors
    ///
    /// Returns `unavailable` when admission is closed, or `limit_exceeded`
    /// when the session bound is full.
    pub fn open_session(
        &self,
        principal: ProductPrincipal,
        authorization: ProductAuthorization,
    ) -> Result<NativeProductClient, ProductError> {
        let session_id = {
            let mut next = self
                .shared
                .next_session_id
                .lock()
                .map_err(|_| unavailable())?;
            let id = ProductSessionId::new(*next)
                .ok_or_else(|| ProductError::from_code(ProductErrorCode::LimitExceeded))?;
            let following = next
                .checked_add(1)
                .ok_or_else(|| ProductError::from_code(ProductErrorCode::LimitExceeded))?;
            *next = following;
            id
        };
        let (reply, receive) = mpsc::sync_channel(1);
        self.send(ServiceCommand::OpenSession {
            session_id,
            principal: principal.clone(),
            authorization,
            reply,
        })?;
        receive.recv().map_err(|_| unavailable())??;
        Ok(NativeProductClient {
            handle: self.clone(),
            session_id,
            principal,
            authorization,
        })
    }

    fn send(&self, command: ServiceCommand) -> Result<(), ProductError> {
        let mut admission = self.shared.admission.lock().map_err(|_| unavailable())?;
        if !self.shared.accepting.load(Ordering::Acquire) {
            return Err(unavailable());
        }
        admission.reserve_owner()?;
        let dispatch = matches!(command, ServiceCommand::Dispatch { .. });
        if dispatch {
            self.increment_queue_depth();
        }
        match self.shared.sender.try_send(command) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                if dispatch {
                    self.decrement_queue_depth();
                }
                admission.retire_owner()?;
                Err(unavailable())
            }
        }
    }

    fn increment_queue_depth(&self) {
        let depth = self
            .shared
            .queue_depth
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        self.shared
            .telemetry
            .set_gauge(MetricId::SchedulerSaturation, depth);
    }

    fn decrement_queue_depth(&self) {
        let depth = self
            .shared
            .queue_depth
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                Some(value.saturating_sub(1))
            })
            .unwrap_or(0)
            .saturating_sub(1);
        self.shared
            .telemetry
            .set_gauge(MetricId::SchedulerSaturation, depth);
    }

    fn dispatch_or_enqueue(
        &self,
        session_id: ProductSessionId,
        context: ProductRequestContext,
        operation: ProductOperation,
        reply: DispatchReply,
    ) -> Result<(), ProductError> {
        #[cfg(test)]
        self.signal_dispatch_attempt();
        let request_id = context.request_id;
        let mut admission = self
            .shared
            .admission
            .lock()
            .map_err(|_| unavailable().with_request_id(request_id))?;
        if !self.shared.accepting.load(Ordering::Acquire) {
            return Err(unavailable().with_request_id(request_id));
        }
        let structure_get = matches!(operation, ProductOperation::StructureGet { .. });
        if !structure_get {
            return self.enqueue_dispatch(&mut admission, session_id, context, operation, reply);
        }
        if admission.pending_owner_commands != 0 {
            #[cfg(test)]
            {
                self.shared
                    .fast_structure_get_fallbacks
                    .fetch_add(1, Ordering::Relaxed);
                self.pause_before_fallback_enqueue();
            }
            return self.enqueue_dispatch(&mut admission, session_id, context, operation, reply);
        }
        let product = self
            .shared
            .product
            .read()
            .map_err(|_| unavailable().with_request_id(request_id))?;
        let sessions = self
            .shared
            .sessions
            .lock()
            .map_err(|_| unavailable().with_request_id(request_id))?;
        let Some(session) = sessions.get(&session_id).cloned() else {
            return Err(ProductError::from_code(ProductErrorCode::InvalidRequest)
                .with_request_id(context.request_id));
        };
        let session = session
            .read()
            .map_err(|_| unavailable().with_request_id(request_id))?;
        if session.has_active_transactions() {
            #[cfg(test)]
            self.shared
                .fast_structure_get_fallbacks
                .fetch_add(1, Ordering::Relaxed);
            #[cfg(test)]
            self.pause_before_fallback_enqueue();
            drop(session);
            drop(sessions);
            drop(product);
            return self.enqueue_dispatch(&mut admission, session_id, context, operation, reply);
        }
        let product = product
            .as_ref()
            .ok_or_else(|| unavailable().with_request_id(request_id))?;
        drop(sessions);
        drop(admission);
        #[cfg(test)]
        self.pause_before_fast_execute();
        product
            .telemetry
            .record_timing(TimingClass::Queueing, std::time::Duration::ZERO);
        #[cfg(test)]
        self.shared
            .fast_structure_get_hits
            .fetch_add(1, Ordering::Relaxed);
        let result = crate::operation::dispatch_structure_get_read_only(
            product, &session, &context, &operation,
        );
        reply.send(result);
        Ok(())
    }

    fn enqueue_dispatch(
        &self,
        admission: &mut ServiceAdmission,
        session_id: ProductSessionId,
        context: ProductRequestContext,
        operation: ProductOperation,
        reply: DispatchReply,
    ) -> Result<(), ProductError> {
        let request_id = context.request_id;
        admission.reserve_owner()?;
        self.increment_queue_depth();
        let command = ServiceCommand::Dispatch {
            session_id,
            context,
            operation: Box::new(operation),
            enqueued_at: Instant::now(),
            reply,
        };
        match self.shared.sender.try_send(command) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                admission.retire_owner()?;
                self.decrement_queue_depth();
                Err(unavailable().with_request_id(request_id))
            }
        }
    }

    #[cfg(test)]
    fn signal_dispatch_attempt(&self) {
        if let Ok(mut hooks) = self.shared.test_hooks.lock()
            && let Some(signal) = hooks.dispatch_attempt.take()
        {
            let _ignored = signal.send(());
        }
    }

    #[cfg(test)]
    fn pause_before_fallback_enqueue(&self) {
        let pause = self
            .shared
            .test_hooks
            .lock()
            .ok()
            .and_then(|mut hooks| hooks.fallback_enqueue_pause.take());
        if let Some(pause) = pause {
            pause.wait();
        }
    }

    #[cfg(test)]
    fn pause_before_fast_execute(&self) {
        let pause = self
            .shared
            .test_hooks
            .lock()
            .ok()
            .and_then(|mut hooks| hooks.fast_execute_pause.take());
        if let Some(pause) = pause {
            pause.wait();
        }
    }
}

/// One service client with independent session-local prepared handles.
pub struct NativeProductClient {
    handle: NativeProductHandle,
    session_id: ProductSessionId,
    principal: ProductPrincipal,
    authorization: ProductAuthorization,
}

impl Drop for NativeProductClient {
    fn drop(&mut self) {
        let (reply, _receive) = mpsc::sync_channel(1);
        let _ignored = self.handle.send(ServiceCommand::CloseSession {
            session_id: self.session_id,
            reply,
        });
    }
}

impl NativeProductClient {
    /// Returns this client's process-local session identity.
    pub const fn session_id(&self) -> ProductSessionId {
        self.session_id
    }

    /// Records one fixed transport-adapter timing class without labels.
    pub fn record_timing(&self, class: TimingClass, duration: std::time::Duration) {
        self.handle.shared.telemetry.record_timing(class, duration);
    }

    /// Constructs a request context bound to this authenticated session.
    pub fn request_context(
        &self,
        request_id: u128,
        logical_time_micros: i64,
    ) -> ProductRequestContext {
        ProductRequestContext::new(
            request_id,
            self.session_id,
            logical_time_micros,
            self.principal.clone(),
            self.authorization,
        )
    }

    /// Admits and waits for one operation result.
    ///
    /// # Errors
    ///
    /// Returns an admission or operation error.
    pub fn dispatch(
        &self,
        context: ProductRequestContext,
        operation: ProductOperation,
    ) -> Result<ProductResponse, ProductError> {
        context.checkpoint()?;
        self.submit(context, operation)?.wait()
    }

    /// Admits without waiting for queue capacity, then waits for execution.
    ///
    /// # Errors
    ///
    /// Returns `unavailable` for queue saturation or any operation error.
    pub fn try_dispatch(
        &self,
        context: ProductRequestContext,
        operation: ProductOperation,
    ) -> Result<ProductResponse, ProductError> {
        context.checkpoint()?;
        self.try_submit(context, operation)?.wait()
    }

    /// Admits one operation and returns its process-local completion receiver.
    ///
    /// # Errors
    ///
    /// Returns an error when the context is foreign or service admission fails.
    pub fn submit(
        &self,
        context: ProductRequestContext,
        operation: ProductOperation,
    ) -> Result<NativeProductPendingResponse, ProductError> {
        self.validate_context(&context)?;
        let request_id = context.request_id;
        let (reply, receive) = mpsc::sync_channel(1);
        self.handle.dispatch_or_enqueue(
            self.session_id,
            context,
            operation,
            DispatchReply::Blocking(reply),
        )?;
        Ok(NativeProductPendingResponse {
            receive,
            request_id,
        })
    }

    /// Admits one operation and returns an asynchronous completion receiver.
    ///
    /// The sole product owner remains synchronous. This completion path lets
    /// async transport adapters await its result without occupying a blocking
    /// executor thread per in-flight request.
    ///
    /// # Errors
    ///
    /// Returns an error when the context is foreign or service admission fails.
    pub fn submit_async(
        &self,
        context: ProductRequestContext,
        operation: ProductOperation,
    ) -> Result<NativeProductPendingAsyncResponse, ProductError> {
        self.validate_context(&context)?;
        let request_id = context.request_id;
        let (reply, receive) = futures_channel::oneshot::channel();
        self.handle.dispatch_or_enqueue(
            self.session_id,
            context,
            operation,
            DispatchReply::Async(reply),
        )?;
        Ok(NativeProductPendingAsyncResponse {
            receive,
            request_id,
        })
    }

    /// Attempts one nonblocking service admission.
    ///
    /// # Errors
    ///
    /// Returns `unavailable` when the queue is full or admission is closed.
    pub fn try_submit(
        &self,
        context: ProductRequestContext,
        operation: ProductOperation,
    ) -> Result<NativeProductPendingResponse, ProductError> {
        self.validate_context(&context)?;
        let request_id = context.request_id;
        let (reply, receive) = mpsc::sync_channel(1);
        self.handle.dispatch_or_enqueue(
            self.session_id,
            context,
            operation,
            DispatchReply::Blocking(reply),
        )?;
        Ok(NativeProductPendingResponse {
            receive,
            request_id,
        })
    }

    /// Explicitly closes this session and releases retained plans.
    ///
    /// # Errors
    ///
    /// Returns `unavailable` when service admission has closed.
    pub fn close(self) -> Result<(), ProductError> {
        let (reply, receive) = mpsc::sync_channel(1);
        self.handle.send(ServiceCommand::CloseSession {
            session_id: self.session_id,
            reply,
        })?;
        let result = receive.recv().map_err(|_| unavailable());
        std::mem::forget(self);
        result
    }

    fn validate_context(&self, context: &ProductRequestContext) -> Result<(), ProductError> {
        if context.session_id != self.session_id
            || context.principal != self.principal
            || context.authorization != self.authorization
        {
            Err(ProductError::from_code(ProductErrorCode::InvalidRequest)
                .with_request_id(context.request_id))
        } else {
            Ok(())
        }
    }
}

/// Completion of one admitted process-local service operation.
pub struct NativeProductPendingResponse {
    receive: Receiver<Result<ProductResponse, ProductError>>,
    request_id: u128,
}

impl NativeProductPendingResponse {
    /// Waits for this admitted operation to complete.
    ///
    /// # Errors
    ///
    /// Returns the operation error or `unavailable` if the owner terminates.
    pub fn wait(self) -> Result<ProductResponse, ProductError> {
        let request_id = self.request_id;
        self.receive.recv().map_err(|_| {
            ProductError::from_code(ProductErrorCode::Unavailable).with_request_id(request_id)
        })?
    }
}

/// Asynchronous completion of one admitted process-local service operation.
pub struct NativeProductPendingAsyncResponse {
    receive: futures_channel::oneshot::Receiver<Result<ProductResponse, ProductError>>,
    request_id: u128,
}

impl NativeProductPendingAsyncResponse {
    /// Waits asynchronously for this admitted operation to complete.
    ///
    /// # Errors
    ///
    /// Returns the operation error or `unavailable` if the owner terminates.
    pub async fn wait(self) -> Result<ProductResponse, ProductError> {
        let request_id = self.request_id;
        self.receive.await.map_err(|_| {
            ProductError::from_code(ProductErrorCode::Unavailable).with_request_id(request_id)
        })?
    }
}

/// Sole owner and graceful-shutdown guard for a native product service.
pub struct NativeProductService {
    handle: NativeProductHandle,
    worker: Option<JoinHandle<Result<NativeProduct, ProductError>>>,
}

impl NativeProductService {
    /// Starts one owner thread with a bounded multi-producer queue.
    ///
    /// # Errors
    ///
    /// Returns an invalid-request or owner-thread startup error.
    pub fn start(
        product: NativeProduct,
        config: NativeProductServiceConfig,
    ) -> Result<Self, ProductError> {
        config.validate()?;
        let (sender, receiver) = mpsc::sync_channel(config.queue_capacity);
        let telemetry = product.telemetry().clone();
        let product = RwLock::new(Some(product));
        let shared = Arc::new(SharedService {
            sender,
            accepting: AtomicBool::new(true),
            next_session_id: Mutex::new(1),
            admission: Mutex::new(ServiceAdmission::default()),
            telemetry,
            queue_depth: AtomicU64::new(0),
            product,
            sessions: Mutex::new(BTreeMap::new()),
            #[cfg(test)]
            fast_structure_get_hits: AtomicU64::new(0),
            #[cfg(test)]
            fast_structure_get_fallbacks: AtomicU64::new(0),
            #[cfg(test)]
            test_hooks: Mutex::new(ServiceTestHooks::default()),
        });
        let worker_builder = thread::Builder::new().name("hyphae-native-product".to_owned());
        let owner_shared = Arc::clone(&shared);
        let worker = worker_builder
            .spawn(move || owner_loop(receiver, config, owner_shared))
            .map_err(|_| unavailable())?;
        Ok(Self {
            handle: NativeProductHandle { shared },
            worker: Some(worker),
        })
    }

    /// Returns a cloneable multi-client service handle.
    pub fn handle(&self) -> NativeProductHandle {
        self.handle.clone()
    }

    /// Stops admission, drains all admitted work, and returns the sole product.
    ///
    /// # Errors
    ///
    /// Returns `unavailable` if the owner terminates before shutdown completes.
    pub fn shutdown(mut self) -> Result<NativeProduct, ProductError> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> Result<NativeProduct, ProductError> {
        let worker = self.worker.take().ok_or_else(unavailable)?;
        let (reply, receive) = mpsc::sync_channel(1);
        self.handle.shared.accepting.store(false, Ordering::Release);
        let registration = self.register_shutdown_intent();
        let sent = self
            .handle
            .shared
            .sender
            .send(ServiceCommand::Shutdown { reply });
        let rollback = if sent.is_err() && registration.is_ok() {
            self.rollback_shutdown_intent()
        } else {
            Ok(())
        };
        let received = sent
            .map_err(|_| unavailable())
            .and_then(|()| receive.recv().map_err(|_| unavailable()));
        let owner = worker
            .join()
            .map_err(|_| ProductError::from_code(ProductErrorCode::Internal))?;
        registration?;
        rollback?;
        received?;
        owner
    }

    fn register_shutdown_intent(&self) -> Result<(), ProductError> {
        let mut admission = self
            .handle
            .shared
            .admission
            .lock()
            .map_err(|_| unavailable())?;
        admission.reserve_owner()
    }

    fn rollback_shutdown_intent(&self) -> Result<(), ProductError> {
        let mut admission = self
            .handle
            .shared
            .admission
            .lock()
            .map_err(|_| unavailable())?;
        admission.retire_owner()
    }
}

impl Drop for NativeProductService {
    fn drop(&mut self) {
        if self.worker.is_none() {
            return;
        }
        let (reply, receive) = mpsc::sync_channel(1);
        self.handle.shared.accepting.store(false, Ordering::Release);
        let registered = self.register_shutdown_intent().is_ok();
        let sent = self
            .handle
            .shared
            .sender
            .send(ServiceCommand::Shutdown { reply })
            .is_ok();
        if sent {
            let _ignored = receive.recv();
        } else if registered {
            let _ignored = self.rollback_shutdown_intent();
        }
        if let Some(worker) = self.worker.take() {
            let _ignored = worker.join();
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
fn owner_loop(
    receiver: Receiver<ServiceCommand>,
    config: NativeProductServiceConfig,
    shared: Arc<SharedService>,
) -> Result<NativeProduct, ProductError> {
    struct FailClosedOnExit<'a>(&'a AtomicBool);

    impl Drop for FailClosedOnExit<'_> {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }

    let _fail_closed = FailClosedOnExit(&shared.accepting);
    while let Ok(command) = receiver.recv() {
        #[cfg(test)]
        pause_owner_after_dequeue(&shared);
        let mut admission = shared.admission.lock().map_err(|_| unavailable())?;
        let mut product_slot = shared.product.write().map_err(|_| unavailable())?;
        let mut sessions = shared.sessions.lock().map_err(|_| unavailable())?;
        admission.retire_owner()?;
        drop(admission);
        let product = product_slot.as_mut().ok_or_else(unavailable)?;
        let shutdown = matches!(command, ServiceCommand::Shutdown { .. });
        match command {
            ServiceCommand::OpenSession {
                session_id,
                principal,
                authorization,
                reply,
            } => {
                let result = if sessions.len() >= config.max_sessions {
                    Err(ProductError::from_code(ProductErrorCode::LimitExceeded))
                } else {
                    let session = ProductSession::with_limits(
                        session_id,
                        principal,
                        authorization,
                        config.max_prepared_per_session,
                        config.max_transaction_statuses_per_session,
                        config.max_active_transactions_per_session,
                    );
                    sessions.insert(session_id, Arc::new(RwLock::new(session)));
                    Ok(session_id)
                };
                let _ignored = reply.send(result);
            }
            ServiceCommand::Dispatch {
                session_id,
                context,
                operation,
                enqueued_at,
                reply,
            } => {
                product
                    .telemetry
                    .record_timing(TimingClass::Queueing, enqueued_at.elapsed());
                let depth = shared
                    .queue_depth
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                        Some(value.saturating_sub(1))
                    })
                    .unwrap_or(0)
                    .saturating_sub(1);
                product
                    .telemetry
                    .set_gauge(MetricId::SchedulerSaturation, depth);
                let result = sessions.get(&session_id).map_or_else(
                    || {
                        Err(ProductError::from_code(ProductErrorCode::InvalidRequest)
                            .with_request_id(context.request_id))
                    },
                    |session| {
                        let mut session = session
                            .write()
                            .map_err(|_| unavailable().with_request_id(context.request_id))?;
                        product.dispatch(&mut session, &context, *operation)
                    },
                );
                reply.send(result);
            }
            ServiceCommand::CloseSession { session_id, reply } => {
                sessions.remove(&session_id);
                let _ignored = reply.send(());
            }
            ServiceCommand::Shutdown { reply } => {
                let _ignored = reply.send(());
            }
        }
        drop(sessions);
        drop(product_slot);
        if shutdown {
            break;
        }
    }
    let mut product = shared.product.write().map_err(|_| unavailable())?;
    product.take().ok_or_else(unavailable)
}

#[cfg(test)]
fn pause_owner_after_dequeue(shared: &SharedService) {
    let pause = shared
        .test_hooks
        .lock()
        .ok()
        .and_then(|mut hooks| hooks.owner_dequeue_pause.take());
    if let Some(pause) = pause {
        pause.wait();
    }
}

fn unavailable() -> ProductError {
    ProductError::from_code(ProductErrorCode::Unavailable)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::*;
    use crate::{
        MetricValue, ProductCancellationToken, ProductDurabilityPolicy,
        ProductExplicitTransactionStatus, ProductLimits, ProductTransactionSqlMutation,
        ProductValue,
    };

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn create(name: &str) -> std::io::Result<Self> {
            let path = std::env::temp_dir().join(format!(
                "hyphae-product-service-{name}-{}-{}",
                std::process::id(),
                NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path)?;
            Ok(Self(path))
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ignored = fs::remove_dir_all(&self.0);
        }
    }

    fn test_principal() -> ProductPrincipal {
        ProductPrincipal::new("service-fast-read-test").expect("valid principal")
    }

    fn test_session() -> ProductSession {
        ProductSession::new(
            ProductSessionId::new(1).expect("nonzero session"),
            test_principal(),
            ProductAuthorization::ALL,
        )
    }

    fn test_context(
        session: &ProductSession,
        request_id: u128,
        logical_time: i64,
    ) -> ProductRequestContext {
        ProductRequestContext::new(
            request_id,
            session.id(),
            logical_time,
            session.principal().clone(),
            session.authorization(),
        )
    }

    fn metric_value(product: &NativeProduct, id: MetricId) -> MetricValue {
        product
            .telemetry()
            .snapshot(0, None)
            .metrics
            .into_iter()
            .find(|row| row.descriptor.id == id)
            .expect("registered metric")
            .value
    }

    fn metric_count(product: &NativeProduct, id: MetricId) -> u64 {
        match metric_value(product, id) {
            MetricValue::Counter(value) => value,
            MetricValue::Histogram { count, .. } => count,
            MetricValue::Gauge(_) => panic!("metric is not countable"),
        }
    }

    fn test_pause() -> (ServiceTestPause, Receiver<()>, SyncSender<()>) {
        let (entered, entered_receive) = mpsc::sync_channel(1);
        let (release, release_receive) = mpsc::sync_channel(1);
        (
            ServiceTestPause {
                entered,
                release: release_receive,
            },
            entered_receive,
            release,
        )
    }

    fn memory_context(client: &NativeProductClient, request_id: u128) -> ProductRequestContext {
        let mut context = client.request_context(request_id, 0);
        context.durability = ProductDurabilityPolicy::MEMORY;
        context
    }

    #[test]
    fn nonblocking_admission_reports_queue_saturation() -> Result<(), ProductError> {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let handle = NativeProductHandle {
            shared: Arc::new(SharedService {
                sender,
                accepting: AtomicBool::new(true),
                next_session_id: Mutex::new(1),
                admission: Mutex::new(ServiceAdmission::default()),
                telemetry: TelemetryRegistry::default(),
                queue_depth: AtomicU64::new(0),
                product: RwLock::new(None),
                sessions: Mutex::new(BTreeMap::new()),
                fast_structure_get_hits: AtomicU64::new(0),
                fast_structure_get_fallbacks: AtomicU64::new(0),
                test_hooks: Mutex::new(ServiceTestHooks::default()),
            }),
        };
        let (first_reply, _first_receive) = mpsc::sync_channel(1);
        handle.send(ServiceCommand::CloseSession {
            session_id: ProductSessionId::new(1)
                .ok_or_else(|| ProductError::from_code(ProductErrorCode::Internal))?,
            reply: first_reply,
        })?;
        let (second_reply, _second_receive) = mpsc::sync_channel(1);
        let error = match handle.send(ServiceCommand::CloseSession {
            session_id: ProductSessionId::new(2)
                .ok_or_else(|| ProductError::from_code(ProductErrorCode::Internal))?,
            reply: second_reply,
        }) {
            Ok(()) => return Err(ProductError::from_code(ProductErrorCode::Internal)),
            Err(error) => error,
        };
        assert_eq!(error.code(), ProductErrorCode::Unavailable);
        let session = test_session();
        let (dispatch_reply, _dispatch_receive) = mpsc::sync_channel(1);
        let error = handle
            .dispatch_or_enqueue(
                session.id(),
                test_context(&session, 77, 0),
                ProductOperation::Capabilities,
                DispatchReply::Blocking(dispatch_reply),
            )
            .expect_err("full queue must reject dispatch");
        assert_eq!(error.code(), ProductErrorCode::Unavailable);
        assert_eq!(error.request_id(), Some(77));
        assert_eq!(handle.shared.queue_depth.load(Ordering::Acquire), 0);
        assert_eq!(
            handle
                .shared
                .admission
                .lock()
                .expect("admission")
                .pending_owner_commands,
            1
        );
        Ok(())
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn immutable_structure_get_reuses_exact_dispatch_contract()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = TemporaryDirectory::create("fast-equivalence")?;
        let mut product = NativeProduct::create(directory.0.join("data"))?;
        let mut session = test_session();
        let mut seed_context = test_context(&session, 1, 10);
        seed_context.durability = ProductDurabilityPolicy::MEMORY;
        product.dispatch(
            &mut session,
            &seed_context,
            ProductOperation::StructureSet {
                key: b"visible".to_vec(),
                value: vec![0x3c; 64],
                expires_at_micros: Some(100),
            },
        )?;

        for (logical_time, key) in [
            (99_i64, b"visible".as_slice()),
            (100_i64, b"visible".as_slice()),
            (99_i64, b"missing".as_slice()),
        ] {
            let direct_context = test_context(
                &session,
                100 + u128::from(logical_time.cast_unsigned()),
                logical_time,
            );
            let fast_context = test_context(
                &session,
                200 + u128::from(logical_time.cast_unsigned()),
                logical_time,
            );
            let operation = ProductOperation::StructureGet { key: key.to_vec() };
            let direct = product.dispatch(&mut session, &direct_context, operation.clone())?;
            let fast = crate::operation::dispatch_structure_get_read_only(
                &product,
                &session,
                &fast_context,
                &operation,
            )?;
            assert_eq!(format!("{direct:?}"), format!("{fast:?}"));
        }

        let requests_before = metric_count(&product, MetricId::Requests);
        let errors_before = metric_count(&product, MetricId::Errors);
        let cancellations_before = metric_count(&product, MetricId::Cancellations);
        let admission_before = metric_count(&product, MetricId::AdmissionMicros);
        let execution_before = metric_count(&product, MetricId::EngineExecutionMicros);
        let mut cancelled = test_context(&session, 400, 99);
        cancelled.cancellation = ProductCancellationToken::new();
        cancelled.cancellation.cancel();
        let error = crate::operation::dispatch_structure_get_read_only(
            &product,
            &session,
            &cancelled,
            &ProductOperation::StructureGet {
                key: b"visible".to_vec(),
            },
        )
        .expect_err("cancelled fast read must fail");
        assert_eq!(error.code(), ProductErrorCode::Cancelled);
        assert_eq!(error.request_id(), Some(400));
        assert_eq!(
            metric_count(&product, MetricId::Requests),
            requests_before + 1
        );
        assert_eq!(metric_count(&product, MetricId::Errors), errors_before + 1);
        assert_eq!(
            metric_count(&product, MetricId::Cancellations),
            cancellations_before + 1
        );
        assert_eq!(
            metric_count(&product, MetricId::AdmissionMicros),
            admission_before + 1
        );
        assert_eq!(
            metric_count(&product, MetricId::EngineExecutionMicros),
            execution_before
        );

        let mut limited = test_context(&session, 401, 99);
        limited.limits = ProductLimits {
            max_response_bytes: 1,
            ..ProductLimits::default()
        };
        let error = crate::operation::dispatch_structure_get_read_only(
            &product,
            &session,
            &limited,
            &ProductOperation::StructureGet {
                key: b"visible".to_vec(),
            },
        )
        .expect_err("response limit must be exact");
        assert_eq!(error.code(), ProductErrorCode::LimitExceeded);
        assert_eq!(error.request_id(), Some(401));

        let mut foreign = test_context(&session, 402, 99);
        foreign.principal = ProductPrincipal::new("foreign").expect("valid foreign principal");
        let error = crate::operation::dispatch_structure_get_read_only(
            &product,
            &session,
            &foreign,
            &ProductOperation::StructureGet {
                key: b"visible".to_vec(),
            },
        )
        .expect_err("foreign authority must fail");
        assert_eq!(error.code(), ProductErrorCode::InvalidRequest);
        assert_eq!(error.request_id(), Some(402));

        let denied_session = ProductSession::new(
            ProductSessionId::new(2).expect("nonzero session"),
            test_principal(),
            ProductAuthorization::NONE,
        );
        let denied_context = test_context(&denied_session, 403, 99);
        let error = crate::operation::dispatch_structure_get_read_only(
            &product,
            &denied_session,
            &denied_context,
            &ProductOperation::StructureGet {
                key: b"visible".to_vec(),
            },
        )
        .expect_err("unauthorized fast read must fail");
        assert_eq!(error.code(), ProductErrorCode::AuthorizationDenied);
        assert_eq!(error.request_id(), Some(403));

        let mut expired_deadline = test_context(&session, 404, 99);
        expired_deadline.deadline_micros = Some(0);
        let error = crate::operation::dispatch_structure_get_read_only(
            &product,
            &session,
            &expired_deadline,
            &ProductOperation::StructureGet {
                key: b"visible".to_vec(),
            },
        )
        .expect_err("elapsed deadline must fail");
        assert_eq!(error.code(), ProductErrorCode::DeadlineExceeded);
        assert_eq!(error.request_id(), Some(404));
        Ok(())
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn active_transaction_forces_owner_then_rollback_restores_fast_read()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = TemporaryDirectory::create("fast-transaction")?;
        let service = NativeProductService::start(
            NativeProduct::create(directory.0.join("data"))?,
            NativeProductServiceConfig::default(),
        )?;
        let client = service
            .handle()
            .open_session(test_principal(), ProductAuthorization::ALL)?;
        let baseline_hits = service
            .handle
            .shared
            .fast_structure_get_hits
            .load(Ordering::Acquire);
        let response = client.dispatch(
            client.request_context(1, 0),
            ProductOperation::StructureGet {
                key: b"missing".to_vec(),
            },
        )?;
        assert!(matches!(response, ProductResponse::StructureValue(None)));
        assert_eq!(
            service
                .handle
                .shared
                .fast_structure_get_hits
                .load(Ordering::Acquire),
            baseline_hits + 1
        );

        let response = client.dispatch(
            client.request_context(2, 0),
            ProductOperation::TransactionBegin,
        )?;
        let ProductResponse::ExplicitTransactionStatus(ProductExplicitTransactionStatus::Active {
            handle,
            ..
        }) = response
        else {
            return Err("transaction did not begin".into());
        };
        let fallbacks_before = service
            .handle
            .shared
            .fast_structure_get_fallbacks
            .load(Ordering::Acquire);
        let response = client.dispatch(
            client.request_context(3, 0),
            ProductOperation::StructureGet {
                key: b"missing".to_vec(),
            },
        )?;
        assert!(matches!(response, ProductResponse::StructureValue(None)));
        assert_eq!(
            service
                .handle
                .shared
                .fast_structure_get_fallbacks
                .load(Ordering::Acquire),
            fallbacks_before + 1
        );
        assert!(matches!(
            client.dispatch(
                client.request_context(4, 0),
                ProductOperation::TransactionRollback { handle },
            )?,
            ProductResponse::TransactionRolledBack(_)
        ));

        let hits_before = service
            .handle
            .shared
            .fast_structure_get_hits
            .load(Ordering::Acquire);
        let response = client.dispatch(
            client.request_context(5, 0),
            ProductOperation::StructureGet {
                key: b"missing".to_vec(),
            },
        )?;
        assert!(matches!(response, ProductResponse::StructureValue(None)));
        assert_eq!(
            service
                .handle
                .shared
                .fast_structure_get_hits
                .load(Ordering::Acquire),
            hits_before + 1
        );

        assert!(matches!(
            client.dispatch(
                memory_context(&client, 6),
                ProductOperation::ExecuteSql {
                    statement: "CREATE TABLE fast_tx (id BIGINT PRIMARY KEY)".to_owned(),
                    parameters: Vec::new(),
                },
            )?,
            ProductResponse::Sql { .. }
        ));
        let response = client.dispatch(
            memory_context(&client, 7),
            ProductOperation::TransactionBegin,
        )?;
        let ProductResponse::ExplicitTransactionStatus(ProductExplicitTransactionStatus::Active {
            handle,
            ..
        }) = response
        else {
            return Err("second transaction did not begin".into());
        };
        assert!(matches!(
            client.dispatch(
                memory_context(&client, 8),
                ProductOperation::TransactionStageSql {
                    handle,
                    mutation: ProductTransactionSqlMutation {
                        statement: "INSERT INTO fast_tx (id) VALUES (?)".to_owned(),
                        parameters: vec![ProductValue::Signed(1)],
                    },
                },
            )?,
            ProductResponse::TransactionStaged(_)
        ));
        assert!(matches!(
            client.dispatch(
                memory_context(&client, 9),
                ProductOperation::TransactionCommit { handle },
            )?,
            ProductResponse::TransactionCommitted(_)
        ));
        let hits_before = service
            .handle
            .shared
            .fast_structure_get_hits
            .load(Ordering::Acquire);
        assert!(matches!(
            client.dispatch(
                client.request_context(10, 0),
                ProductOperation::StructureGet {
                    key: b"missing-after-commit".to_vec(),
                },
            )?,
            ProductResponse::StructureValue(None)
        ));
        assert_eq!(
            service
                .handle
                .shared
                .fast_structure_get_hits
                .load(Ordering::Acquire),
            hits_before + 1
        );
        client.close()?;
        drop(service.shutdown()?);
        Ok(())
    }

    #[test]
    fn fallback_reserves_owner_order_before_releasing_admission()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = TemporaryDirectory::create("fast-order")?;
        let service = NativeProductService::start(
            NativeProduct::create(directory.0.join("data"))?,
            NativeProductServiceConfig::default(),
        )?;
        let handle = service.handle();
        let writer = handle.open_session(test_principal(), ProductAuthorization::ALL)?;
        let earlier_reader = handle.open_session(test_principal(), ProductAuthorization::ALL)?;
        let later_reader = handle.open_session(test_principal(), ProductAuthorization::ALL)?;

        let (owner_pause, owner_entered, owner_release) = test_pause();
        handle
            .shared
            .test_hooks
            .lock()
            .expect("test hooks")
            .owner_dequeue_pause = Some(owner_pause);
        let mutation = writer.submit(
            memory_context(&writer, 1),
            ProductOperation::StructureSet {
                key: b"ordered".to_vec(),
                value: b"visible".to_vec(),
                expires_at_micros: None,
            },
        )?;
        owner_entered.recv_timeout(std::time::Duration::from_secs(2))?;

        let (fallback_pause, fallback_entered, fallback_release) = test_pause();
        handle
            .shared
            .test_hooks
            .lock()
            .expect("test hooks")
            .fallback_enqueue_pause = Some(fallback_pause);
        let earlier = thread::spawn(move || {
            earlier_reader.dispatch(
                earlier_reader.request_context(2, 0),
                ProductOperation::StructureGet {
                    key: b"ordered".to_vec(),
                },
            )
        });
        fallback_entered.recv_timeout(std::time::Duration::from_secs(2))?;
        assert!(handle.shared.admission.try_lock().is_err());

        let (attempt, attempted) = mpsc::sync_channel(1);
        handle
            .shared
            .test_hooks
            .lock()
            .expect("test hooks")
            .dispatch_attempt = Some(attempt);
        let later = thread::spawn(move || {
            later_reader.dispatch(
                later_reader.request_context(3, 0),
                ProductOperation::StructureGet {
                    key: b"ordered".to_vec(),
                },
            )
        });
        attempted.recv_timeout(std::time::Duration::from_secs(2))?;
        assert!(handle.shared.admission.try_lock().is_err());
        fallback_release.send(())?;

        let mut all_reserved = false;
        for _ in 0..100_000 {
            if handle.shared.queue_depth.load(Ordering::Acquire) == 3 {
                all_reserved = true;
                break;
            }
            thread::yield_now();
        }
        assert!(all_reserved, "both fallback reads must reserve owner order");
        owner_release.send(())?;
        assert!(matches!(mutation.wait()?, ProductResponse::StructureSet(_)));
        for result in [
            earlier.join().map_err(|_| "earlier reader panicked")??,
            later.join().map_err(|_| "later reader panicked")??,
        ] {
            assert!(matches!(
                result,
                ProductResponse::StructureValue(Some(value)) if value == b"visible"
            ));
        }
        drop(service.shutdown()?);
        Ok(())
    }

    #[test]
    fn closed_admission_rejects_fast_read_without_owner_fallback()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = TemporaryDirectory::create("fast-closed")?;
        let service = NativeProductService::start(
            NativeProduct::create(directory.0.join("data"))?,
            NativeProductServiceConfig::default(),
        )?;
        let client = service
            .handle()
            .open_session(test_principal(), ProductAuthorization::ALL)?;
        service
            .handle
            .shared
            .accepting
            .store(false, Ordering::Release);
        let Err(error) = client.submit(
            client.request_context(91, 0),
            ProductOperation::StructureGet {
                key: b"closed".to_vec(),
            },
        ) else {
            return Err("closed service admitted a fast read".into());
        };
        assert_eq!(error.code(), ProductErrorCode::Unavailable);
        assert_eq!(error.request_id(), Some(91));
        assert_eq!(
            service
                .handle
                .shared
                .fast_structure_get_fallbacks
                .load(Ordering::Acquire),
            0
        );
        drop(client);
        drop(service.shutdown()?);
        Ok(())
    }

    #[test]
    fn queue_full_shutdown_waits_for_fast_lease_without_deadlock()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = TemporaryDirectory::create("fast-shutdown")?;
        let service = NativeProductService::start(
            NativeProduct::create(directory.0.join("data"))?,
            NativeProductServiceConfig {
                queue_capacity: 1,
                ..NativeProductServiceConfig::default()
            },
        )?;
        let handle = service.handle();
        let fast_client = handle.open_session(test_principal(), ProductAuthorization::ALL)?;
        let first_owner = handle.open_session(test_principal(), ProductAuthorization::ALL)?;
        let second_owner = handle.open_session(test_principal(), ProductAuthorization::ALL)?;

        let (fast_pause, fast_entered, fast_release) = test_pause();
        handle
            .shared
            .test_hooks
            .lock()
            .expect("test hooks")
            .fast_execute_pause = Some(fast_pause);
        let fast = thread::spawn(move || {
            fast_client.dispatch(
                fast_client.request_context(1, 0),
                ProductOperation::StructureGet {
                    key: b"missing".to_vec(),
                },
            )
        });
        fast_entered.recv_timeout(std::time::Duration::from_secs(2))?;

        let (owner_pause, owner_entered, owner_release) = test_pause();
        handle
            .shared
            .test_hooks
            .lock()
            .expect("test hooks")
            .owner_dequeue_pause = Some(owner_pause);
        let first = first_owner.submit(
            first_owner.request_context(2, 0),
            ProductOperation::Capabilities,
        )?;
        owner_entered.recv_timeout(std::time::Duration::from_secs(2))?;
        let second = second_owner.submit(
            second_owner.request_context(3, 0),
            ProductOperation::Capabilities,
        )?;

        let accepting = Arc::clone(&handle.shared);
        let (shutdown_done, shutdown_receive) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let _ignored = shutdown_done.send(service.shutdown());
        });
        while accepting.accepting.load(Ordering::Acquire) {
            thread::yield_now();
        }
        assert!(matches!(
            shutdown_receive.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        owner_release.send(())?;
        assert!(matches!(
            shutdown_receive.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        fast_release.send(())?;
        assert!(matches!(
            fast.join().map_err(|_| "fast reader panicked")??,
            ProductResponse::StructureValue(None)
        ));
        assert!(matches!(first.wait()?, ProductResponse::Capabilities(_)));
        assert!(matches!(second.wait()?, ProductResponse::Capabilities(_)));
        drop(shutdown_receive.recv_timeout(std::time::Duration::from_secs(2))??);
        Ok(())
    }

    #[test]
    fn poisoned_admission_shutdown_joins_failed_owner() -> Result<(), Box<dyn std::error::Error>> {
        let directory = TemporaryDirectory::create("fast-poison")?;
        let service = NativeProductService::start(
            NativeProduct::create(directory.0.join("data"))?,
            NativeProductServiceConfig::default(),
        )?;
        let admission = Arc::clone(&service.handle.shared);
        let poison = thread::spawn(move || {
            let _guard = admission.admission.lock().expect("admission lock");
            panic!("poison admission for fail-closed shutdown");
        });
        assert!(poison.join().is_err());
        let (done, receive) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let _ignored = done.send(service.shutdown());
        });
        let error = receive
            .recv_timeout(std::time::Duration::from_secs(2))?
            .expect_err("poisoned authority must fail closed");
        assert_eq!(error.code(), ProductErrorCode::Unavailable);
        Ok(())
    }

    #[test]
    fn poisoned_product_or_session_authority_rejects_fast_read()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = TemporaryDirectory::create("fast-session-poison")?;
        let service = NativeProductService::start(
            NativeProduct::create(directory.0.join("data"))?,
            NativeProductServiceConfig::default(),
        )?;
        let client = service
            .handle()
            .open_session(test_principal(), ProductAuthorization::ALL)?;
        let session = service
            .handle
            .shared
            .sessions
            .lock()
            .expect("sessions")
            .get(&client.session_id())
            .cloned()
            .expect("service session");
        let poison = thread::spawn(move || {
            let _guard = session.write().expect("session lock");
            panic!("poison session authority");
        });
        assert!(poison.join().is_err());
        let Err(error) = client.submit(
            client.request_context(501, 0),
            ProductOperation::StructureGet {
                key: b"missing".to_vec(),
            },
        ) else {
            return Err("poisoned session admitted a fast read".into());
        };
        assert_eq!(error.code(), ProductErrorCode::Unavailable);
        assert_eq!(error.request_id(), Some(501));
        drop(client);
        drop(service.shutdown()?);

        let directory = TemporaryDirectory::create("fast-product-poison")?;
        let service = NativeProductService::start(
            NativeProduct::create(directory.0.join("data"))?,
            NativeProductServiceConfig::default(),
        )?;
        let client = service
            .handle()
            .open_session(test_principal(), ProductAuthorization::ALL)?;
        let shared = Arc::clone(&service.handle.shared);
        let poison = thread::spawn(move || {
            let _guard = shared.product.write().expect("product lock");
            panic!("poison product authority");
        });
        assert!(poison.join().is_err());
        let Err(error) = client.submit(
            client.request_context(502, 0),
            ProductOperation::StructureGet {
                key: b"missing".to_vec(),
            },
        ) else {
            return Err("poisoned product admitted a fast read".into());
        };
        assert_eq!(error.code(), ProductErrorCode::Unavailable);
        assert_eq!(error.request_id(), Some(502));
        let Err(error) = service.shutdown() else {
            return Err("poisoned product returned from shutdown".into());
        };
        assert_eq!(error.code(), ProductErrorCode::Unavailable);
        drop(client);
        Ok(())
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn concurrent_fast_readers_do_not_starve_owner_at_c8_or_c32()
    -> Result<(), Box<dyn std::error::Error>> {
        for concurrency in [8_usize, 32] {
            let directory = TemporaryDirectory::create(&format!("fast-c{concurrency}"))?;
            let service = NativeProductService::start(
                NativeProduct::create(directory.0.join("data"))?,
                NativeProductServiceConfig::default(),
            )?;
            let handle = service.handle();
            let owner = handle.open_session(test_principal(), ProductAuthorization::ALL)?;
            assert!(matches!(
                owner.dispatch(
                    memory_context(&owner, 1),
                    ProductOperation::StructureSet {
                        key: b"shared".to_vec(),
                        value: b"before".to_vec(),
                        expires_at_micros: None,
                    },
                )?,
                ProductResponse::StructureSet(_)
            ));
            let clients = (0..concurrency)
                .map(|_| {
                    handle
                        .open_session(test_principal(), ProductAuthorization::ALL)
                        .map(Arc::new)
                })
                .collect::<Result<Vec<_>, _>>()?;
            let hits_before = handle
                .shared
                .fast_structure_get_hits
                .load(Ordering::Acquire);
            let fallbacks_before = handle
                .shared
                .fast_structure_get_fallbacks
                .load(Ordering::Acquire);
            let (fast_pause, fast_entered, fast_release) = test_pause();
            handle
                .shared
                .test_hooks
                .lock()
                .expect("test hooks")
                .fast_execute_pause = Some(fast_pause);
            let start = Arc::new(std::sync::Barrier::new(concurrency + 1));
            let (completed, receive) = mpsc::sync_channel(concurrency);
            let mut workers = Vec::with_capacity(concurrency);
            for (worker, client) in clients.iter().enumerate() {
                let start = Arc::clone(&start);
                let completed = completed.clone();
                let client = Arc::clone(client);
                workers.push(thread::spawn(move || {
                    start.wait();
                    let result = (0..64_u128).try_for_each(|iteration| {
                        let response = client.dispatch(
                            client.request_context(10_000 + (worker as u128 * 64) + iteration, 0),
                            ProductOperation::StructureGet {
                                key: b"shared".to_vec(),
                            },
                        )?;
                        match response {
                            ProductResponse::StructureValue(Some(value))
                                if value == b"before" || value == b"after" =>
                            {
                                Ok(())
                            }
                            _ => Err(ProductError::from_code(ProductErrorCode::Internal)),
                        }
                    });
                    let _ignored = completed.send(result);
                }));
            }
            drop(completed);
            start.wait();
            fast_entered.recv_timeout(std::time::Duration::from_secs(2))?;
            let mutation = owner.submit(
                memory_context(&owner, 2),
                ProductOperation::StructureSet {
                    key: b"shared".to_vec(),
                    value: b"after".to_vec(),
                    expires_at_micros: None,
                },
            )?;
            fast_release.send(())?;
            assert!(matches!(mutation.wait()?, ProductResponse::StructureSet(_)));
            for _ in 0..concurrency {
                receive.recv_timeout(std::time::Duration::from_secs(5))??;
            }
            for worker in workers {
                worker.join().map_err(|_| "fast reader panicked")?;
            }
            drop(clients);
            let classified = handle
                .shared
                .fast_structure_get_hits
                .load(Ordering::Acquire)
                .saturating_sub(hits_before)
                + handle
                    .shared
                    .fast_structure_get_fallbacks
                    .load(Ordering::Acquire)
                    .saturating_sub(fallbacks_before);
            assert_eq!(classified, (concurrency * 64) as u64);
            assert!(matches!(
                owner.dispatch(
                    owner.request_context(3, 0),
                    ProductOperation::StructureGet {
                        key: b"shared".to_vec(),
                    },
                )?,
                ProductResponse::StructureValue(Some(value)) if value == b"after"
            ));
            owner.close()?;
            drop(service.shutdown()?);
        }
        Ok(())
    }
}
