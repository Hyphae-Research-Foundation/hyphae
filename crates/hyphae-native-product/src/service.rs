// SPDX-License-Identifier: GPL-3.0-only

//! One-owner bounded multi-client product operation service.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
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
        reply: SyncSender<Result<ProductResponse, ProductError>>,
    },
    CloseSession {
        session_id: ProductSessionId,
        reply: SyncSender<()>,
    },
    Shutdown {
        reply: SyncSender<()>,
    },
}

struct SharedService {
    sender: SyncSender<ServiceCommand>,
    accepting: AtomicBool,
    next_session_id: Mutex<u128>,
    admission: Mutex<()>,
    telemetry: TelemetryRegistry,
    queue_depth: AtomicU64,
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
        let _admission = self.shared.admission.lock().map_err(|_| unavailable())?;
        if !self.shared.accepting.load(Ordering::Acquire) {
            return Err(unavailable());
        }
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
                Err(unavailable())
            }
        }
    }

    fn try_send(&self, command: ServiceCommand) -> Result<(), ProductError> {
        let _admission = self.shared.admission.lock().map_err(|_| unavailable())?;
        if !self.shared.accepting.load(Ordering::Acquire) {
            return Err(unavailable());
        }
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
        let _ignored = self.handle.try_send(ServiceCommand::CloseSession {
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

    /// Enqueues and waits for one operation result.
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

    /// Enqueues without waiting for queue capacity, then waits for execution.
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

    /// Enqueues one operation and returns its process-local completion receiver.
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
        self.handle.send(ServiceCommand::Dispatch {
            session_id: self.session_id,
            context,
            operation: Box::new(operation),
            enqueued_at: Instant::now(),
            reply,
        })?;
        Ok(NativeProductPendingResponse {
            receive,
            request_id,
        })
    }

    /// Attempts one nonblocking queue admission.
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
        self.handle.try_send(ServiceCommand::Dispatch {
            session_id: self.session_id,
            context,
            operation: Box::new(operation),
            enqueued_at: Instant::now(),
            reply,
        })?;
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
    /// Waits for the sole owner to execute this admitted operation.
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

/// Sole owner and graceful-shutdown guard for a native product service.
pub struct NativeProductService {
    handle: NativeProductHandle,
    worker: Option<JoinHandle<NativeProduct>>,
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
        let shared = Arc::new(SharedService {
            sender,
            accepting: AtomicBool::new(true),
            next_session_id: Mutex::new(1),
            admission: Mutex::new(()),
            telemetry,
            queue_depth: AtomicU64::new(0),
        });
        let worker = thread::Builder::new()
            .name("hyphae-native-product".to_owned())
            .spawn({
                let shared = Arc::clone(&shared);
                move || owner_loop(product, receiver, config, shared)
            })
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
        {
            let _admission = self
                .handle
                .shared
                .admission
                .lock()
                .map_err(|_| unavailable())?;
            self.handle.shared.accepting.store(false, Ordering::Release);
            self.handle
                .shared
                .sender
                .send(ServiceCommand::Shutdown { reply })
                .map_err(|_| unavailable())?;
        }
        receive.recv().map_err(|_| unavailable())?;
        worker
            .join()
            .map_err(|_| ProductError::from_code(ProductErrorCode::Internal))
    }
}

impl Drop for NativeProductService {
    fn drop(&mut self) {
        if self.worker.is_none() {
            return;
        }
        let (reply, receive) = mpsc::sync_channel(1);
        let sent = if let Ok(_admission) = self.handle.shared.admission.lock() {
            self.handle.shared.accepting.store(false, Ordering::Release);
            self.handle
                .shared
                .sender
                .send(ServiceCommand::Shutdown { reply })
                .is_ok()
        } else {
            false
        };
        if sent {
            let _ignored = receive.recv();
        }
        if let Some(worker) = self.worker.take() {
            let _ignored = worker.join();
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
fn owner_loop(
    mut product: NativeProduct,
    receiver: Receiver<ServiceCommand>,
    config: NativeProductServiceConfig,
    shared: Arc<SharedService>,
) -> NativeProduct {
    let mut sessions = BTreeMap::new();
    while let Ok(command) = receiver.recv() {
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
                    sessions.insert(
                        session_id,
                        ProductSession::with_limits(
                            session_id,
                            principal,
                            authorization,
                            config.max_prepared_per_session,
                            config.max_transaction_statuses_per_session,
                            config.max_active_transactions_per_session,
                        ),
                    );
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
                let result = sessions.get_mut(&session_id).map_or_else(
                    || {
                        Err(ProductError::from_code(ProductErrorCode::InvalidRequest)
                            .with_request_id(context.request_id))
                    },
                    |session| product.dispatch(session, &context, *operation),
                );
                let _ignored = reply.send(result);
            }
            ServiceCommand::CloseSession { session_id, reply } => {
                sessions.remove(&session_id);
                let _ignored = reply.send(());
            }
            ServiceCommand::Shutdown { reply } => {
                let _ignored = reply.send(());
                break;
            }
        }
    }
    product
}

fn unavailable() -> ProductError {
    ProductError::from_code(ProductErrorCode::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonblocking_admission_reports_queue_saturation() -> Result<(), ProductError> {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let handle = NativeProductHandle {
            shared: Arc::new(SharedService {
                sender,
                accepting: AtomicBool::new(true),
                next_session_id: Mutex::new(1),
                admission: Mutex::new(()),
                telemetry: TelemetryRegistry::default(),
                queue_depth: AtomicU64::new(0),
            }),
        };
        let (first_reply, _first_receive) = mpsc::sync_channel(1);
        handle.try_send(ServiceCommand::CloseSession {
            session_id: ProductSessionId::new(1)
                .ok_or_else(|| ProductError::from_code(ProductErrorCode::Internal))?,
            reply: first_reply,
        })?;
        let (second_reply, _second_receive) = mpsc::sync_channel(1);
        let error = match handle.try_send(ServiceCommand::CloseSession {
            session_id: ProductSessionId::new(2)
                .ok_or_else(|| ProductError::from_code(ProductErrorCode::Internal))?,
            reply: second_reply,
        }) {
            Ok(()) => return Err(ProductError::from_code(ProductErrorCode::Internal)),
            Err(error) => error,
        };
        assert_eq!(error.code(), ProductErrorCode::Unavailable);
        Ok(())
    }
}
