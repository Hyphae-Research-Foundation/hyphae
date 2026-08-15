// SPDX-License-Identifier: AGPL-3.0-only

//! Multi-client UDS and Windows named-pipe adapter over one product service owner.

#![forbid(unsafe_code)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::result_large_err)]

use std::{
    collections::BTreeMap,
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use hyphae_native_product::{
    ApiKeyCredential, NativeProduct, NativeProductClient, NativeProductService,
    NativeProductServiceConfig, ProductAuthorization, ProductError, ProductErrorCode,
    ProductOperation, ProductPermission, ProductPrincipal, ProductResponse, TimingClass,
};
use hyphae_native_protocol::{
    AsyncFrameIo, ControlError, FlowWindow, FrameIoError, FrameKind, HandshakeError, Hello,
    NegotiationPolicy, ProductCodecError, ProtocolCapabilities, StreamCompletion,
    decode_authenticated_hello, decode_cancel, decode_deadline, decode_hello,
    decode_product_request, decode_window_update, encode_end, encode_failure,
    encode_product_response, encode_welcome, negotiate,
};
use interprocess::local_socket::traits::StreamCommon as _;
use interprocess::local_socket::{ListenerOptions, PeerCreds};
use thiserror::Error;
use tokio::{
    sync::{Mutex as AsyncMutex, Notify, broadcast},
    task::{JoinHandle, JoinSet},
};

#[cfg(windows)]
use interprocess::{
    local_socket::ToNsName as _,
    os::windows::{local_socket::ListenerOptionsExt as _, security_descriptor::SecurityDescriptor},
};
#[cfg(unix)]
use {
    interprocess::local_socket::{GenericFilePath, ToFsName as _},
    std::{fs, os::unix::fs::PermissionsExt as _},
};

/// Default maximum concurrently connected clients.
pub const DEFAULT_MAX_CLIENTS: usize = 1_024;
/// Default maximum stream byte window.
pub const DEFAULT_MAXIMUM_WINDOW: u64 = 16 * 1024 * 1024;

#[cfg(windows)]
const WINDOWS_PIPE_PREFIX: &str = r"\\.\pipe\";

/// Daemon transport and admission configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeDaemonConfig {
    /// Maximum frame payload admitted before and after handshake.
    pub maximum_frame_payload: usize,
    /// Maximum simultaneous client connections.
    pub maximum_clients: usize,
    /// Maximum simultaneous active requests per client.
    pub maximum_in_flight: u32,
    /// Maximum flow-control window for any stream.
    pub maximum_window: u64,
    /// Product service queue/session configuration.
    pub product_service: NativeProductServiceConfig,
}

impl Default for NativeDaemonConfig {
    fn default() -> Self {
        Self {
            maximum_frame_payload: hyphae_native_protocol::DEFAULT_MAX_FRAME_PAYLOAD,
            maximum_clients: DEFAULT_MAX_CLIENTS,
            maximum_in_flight: 64,
            maximum_window: DEFAULT_MAXIMUM_WINDOW,
            product_service: NativeProductServiceConfig::default(),
        }
    }
}

/// Authenticated transport identity captured before product-session creation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(clippy::struct_field_names)]
pub struct PeerIdentity {
    /// Peer process identity when the platform supplies it.
    pub process_id: Option<u32>,
    /// Effective Unix user identity when available.
    pub effective_user_id: Option<u32>,
    /// Effective Unix group identity when available.
    pub effective_group_id: Option<u32>,
}

impl PeerIdentity {
    fn from_credentials(credentials: PeerCreds) -> Self {
        Self {
            process_id: peer_process_id(credentials),
            #[cfg(unix)]
            effective_user_id: credentials.euid(),
            #[cfg(unix)]
            effective_group_id: credentials.egid(),
            #[cfg(windows)]
            effective_user_id: None,
            #[cfg(windows)]
            effective_group_id: None,
        }
    }

    fn principal(self, hello: &Hello) -> Result<ProductPrincipal, DaemonError> {
        let identity = format!(
            "local:pid={}:uid={}:gid={}:client={}",
            self.process_id
                .map_or_else(|| "-".to_owned(), |value| value.to_string()),
            self.effective_user_id
                .map_or_else(|| "-".to_owned(), |value| value.to_string()),
            self.effective_group_id
                .map_or_else(|| "-".to_owned(), |value| value.to_string()),
            hello.client_identity
        );
        ProductPrincipal::new(identity).ok_or(DaemonError::InvalidPeerIdentity)
    }
}

#[cfg(unix)]
fn peer_process_id(credentials: PeerCreds) -> Option<u32> {
    credentials
        .pid()
        .and_then(|value| u32::try_from(value).ok())
}

#[cfg(windows)]
fn peer_process_id(credentials: PeerCreds) -> Option<u32> {
    credentials.pid()
}

/// Local daemon startup, protocol, or shutdown failure.
#[derive(Debug, Error)]
#[allow(clippy::large_enum_variant)]
pub enum DaemonError {
    /// Daemon configuration is zero or above a protocol bound.
    #[error("native daemon configuration is invalid")]
    InvalidConfiguration,
    /// Endpoint identity is malformed or already in use.
    #[error("native daemon endpoint is invalid or already in use")]
    InvalidEndpoint,
    /// OS peer credentials could not form a bounded principal.
    #[error("native daemon peer identity is invalid")]
    InvalidPeerIdentity,
    /// A connected client violated session or correlation rules.
    #[error("native daemon client protocol state is invalid")]
    ClientProtocol,
    /// Endpoint or task I/O failed.
    #[error("native daemon I/O failed: {0}")]
    Io(#[from] io::Error),
    /// Native frame I/O failed.
    #[error(transparent)]
    Frame(#[from] FrameIoError),
    /// Handshake failed.
    #[error(transparent)]
    Handshake(#[from] HandshakeError),
    /// Product codec failed.
    #[error(transparent)]
    ProductCodec(#[from] ProductCodecError),
    /// Flow control failed.
    #[error(transparent)]
    Control(#[from] ControlError),
    /// Product service rejected startup or shutdown.
    #[error("native product service failed: {0}")]
    Product(#[from] ProductError),
    /// A daemon task did not stop cleanly.
    #[error("native daemon task failed")]
    Task,
}

/// Graceful-shutdown owner for the listener and sole product service.
pub struct NativeDaemon {
    endpoint: String,
    accepting: Arc<AtomicBool>,
    shutdown: broadcast::Sender<()>,
    listener: Option<JoinHandle<Result<(), DaemonError>>>,
    service: Option<NativeProductService>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DaemonAuthentication {
    UnmanagedPeer,
    ManagedApiKey,
}

enum ConnectionCredential {
    Unmanaged {
        principal: ProductPrincipal,
        authorization: ProductAuthorization,
    },
    Managed(ApiKeyCredential),
}

impl NativeDaemon {
    /// Binds the platform local endpoint and starts multi-client admission.
    ///
    /// Unix endpoints are filesystem UDS paths with mode `0600`. Windows
    /// endpoints are named-pipe namespace identities. The safe audited
    /// `interprocess` wrapper supplies the stable named-pipe server and peer
    /// credential implementation absent from `std`.
    pub fn start(
        product: NativeProduct,
        endpoint: impl Into<String>,
        config: NativeDaemonConfig,
    ) -> Result<Self, DaemonError> {
        let service = NativeProductService::start(product, config.product_service)?;
        Self::start_with_service(service, endpoint, config)
    }

    /// Binds one daemon that requires a durable Native API key in `HELLO`.
    pub fn start_authenticated(
        product: NativeProduct,
        endpoint: impl Into<String>,
        config: NativeDaemonConfig,
    ) -> Result<Self, DaemonError> {
        let service = NativeProductService::start(product, config.product_service)?;
        Self::start_with_service_authenticated(service, endpoint, config)
    }

    /// Binds the platform local endpoint around an already-started sole
    /// product service. Edge adapters can clone the service handle before this
    /// call so every listener dispatches through the same product owner.
    pub fn start_with_service(
        service: NativeProductService,
        endpoint: impl Into<String>,
        config: NativeDaemonConfig,
    ) -> Result<Self, DaemonError> {
        Self::start_with_service_policy(
            service,
            endpoint,
            config,
            DaemonAuthentication::UnmanagedPeer,
            None,
        )
    }

    /// Binds one API-key-authenticated daemon around an existing product owner.
    pub fn start_with_service_authenticated(
        service: NativeProductService,
        endpoint: impl Into<String>,
        config: NativeDaemonConfig,
    ) -> Result<Self, DaemonError> {
        Self::start_with_service_policy(
            service,
            endpoint,
            config,
            DaemonAuthentication::ManagedApiKey,
            None,
        )
    }

    /// Starts a real daemon that denies data reads for one acceptance-test identity.
    #[doc(hidden)]
    pub fn start_with_service_for_acceptance(
        service: NativeProductService,
        endpoint: impl Into<String>,
        config: NativeDaemonConfig,
        denied_client_identity: impl Into<String>,
    ) -> Result<Self, DaemonError> {
        Self::start_with_service_policy(
            service,
            endpoint,
            config,
            DaemonAuthentication::UnmanagedPeer,
            Some(denied_client_identity.into()),
        )
    }

    fn start_with_service_policy(
        service: NativeProductService,
        endpoint: impl Into<String>,
        config: NativeDaemonConfig,
        authentication: DaemonAuthentication,
        denied_client_identity: Option<String>,
    ) -> Result<Self, DaemonError> {
        validate_config(config)?;
        let endpoint = endpoint.into();
        let endpoint = normalize_endpoint(&endpoint)?;
        let handle = service.handle();
        let listener = create_listener(&endpoint)?;
        let (shutdown, shutdown_receive) = broadcast::channel(1);
        let accepting = Arc::new(AtomicBool::new(true));
        let listener_accepting = accepting.clone();
        let listener_task = tokio::spawn(async move {
            listener_loop(
                listener,
                handle,
                config,
                authentication,
                denied_client_identity,
                listener_accepting,
                shutdown_receive,
            )
            .await
        });
        Ok(Self {
            endpoint,
            accepting,
            shutdown,
            listener: Some(listener_task),
            service: Some(service),
        })
    }

    /// Returns the exact bound endpoint identity.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Stops admission, drains connections and admitted product work, and
    /// returns the sole product owner.
    pub async fn shutdown(mut self) -> Result<NativeProduct, DaemonError> {
        self.accepting.store(false, Ordering::Release);
        let _ignored = self.shutdown.send(());
        if let Some(listener) = self.listener.take() {
            listener.await.map_err(|_| DaemonError::Task)??;
        }
        let service = self.service.take().ok_or(DaemonError::Task)?;
        service.shutdown().map_err(Into::into)
    }
}

impl Drop for NativeDaemon {
    fn drop(&mut self) {
        self.accepting.store(false, Ordering::Release);
        let _ignored = self.shutdown.send(());
    }
}

/// Connects a portable client stream to a UDS path on Unix or a named-pipe
/// namespace endpoint on Windows.
pub async fn connect(
    endpoint: &str,
) -> Result<interprocess::local_socket::tokio::Stream, DaemonError> {
    use interprocess::local_socket::tokio::prelude::*;

    #[cfg(unix)]
    let name = endpoint
        .to_fs_name::<GenericFilePath>()
        .map_err(|_| DaemonError::InvalidEndpoint)?;
    #[cfg(windows)]
    let endpoint = normalize_windows_endpoint(endpoint)?;
    #[cfg(windows)]
    let name = endpoint
        .as_str()
        .to_ns_name::<interprocess::local_socket::GenericNamespaced>()
        .map_err(|_| DaemonError::InvalidEndpoint)?;
    interprocess::local_socket::tokio::Stream::connect(name)
        .await
        .map_err(DaemonError::Io)
}

fn normalize_endpoint(endpoint: &str) -> Result<String, DaemonError> {
    #[cfg(unix)]
    {
        if endpoint.is_empty() {
            Err(DaemonError::InvalidEndpoint)
        } else {
            Ok(endpoint.to_owned())
        }
    }
    #[cfg(windows)]
    {
        normalize_windows_endpoint(endpoint)
    }
}

#[cfg(windows)]
fn normalize_windows_endpoint(endpoint: &str) -> Result<String, DaemonError> {
    let bare = endpoint
        .get(..WINDOWS_PIPE_PREFIX.len())
        .filter(|prefix| prefix.eq_ignore_ascii_case(WINDOWS_PIPE_PREFIX))
        .map_or(endpoint, |_| &endpoint[WINDOWS_PIPE_PREFIX.len()..]);
    if bare.is_empty() || bare.starts_with(r"\\") {
        return Err(DaemonError::InvalidEndpoint);
    }
    Ok(bare.to_owned())
}

fn validate_config(config: NativeDaemonConfig) -> Result<(), DaemonError> {
    if config.maximum_frame_payload == 0
        || config.maximum_frame_payload > hyphae_native_protocol::DEFAULT_MAX_FRAME_PAYLOAD
        || config.maximum_clients == 0
        || config.maximum_in_flight == 0
        || config.maximum_window == 0
        || config.maximum_window > u64::from(u32::MAX)
    {
        Err(DaemonError::InvalidConfiguration)
    } else {
        Ok(())
    }
}

fn create_listener(
    endpoint: &str,
) -> Result<interprocess::local_socket::tokio::Listener, DaemonError> {
    #[cfg(unix)]
    {
        let name = endpoint
            .to_fs_name::<GenericFilePath>()
            .map_err(|_| DaemonError::InvalidEndpoint)?;
        let listener = ListenerOptions::new()
            .name(name)
            .reclaim_name(true)
            .try_overwrite(false)
            .create_tokio()
            .map_err(DaemonError::Io)?;
        fs::set_permissions(endpoint, fs::Permissions::from_mode(0o600))?;
        Ok(listener)
    }
    #[cfg(windows)]
    {
        let name = endpoint
            .to_ns_name::<interprocess::local_socket::GenericNamespaced>()
            .map_err(|_| DaemonError::InvalidEndpoint)?;
        ListenerOptions::new()
            .name(name)
            .reclaim_name(false)
            .security_descriptor(windows_security_descriptor()?)
            .create_tokio()
            .map_err(DaemonError::Io)
    }
}

#[cfg(windows)]
fn windows_security_descriptor() -> Result<SecurityDescriptor, DaemonError> {
    // The protected DACL grants access only to the object owner and LocalSystem.
    let descriptor = widestring::U16CString::from_str("D:P(A;;GA;;;OW)(A;;GA;;;SY)")
        .map_err(|_| DaemonError::InvalidEndpoint)?;
    SecurityDescriptor::deserialize(&descriptor).map_err(DaemonError::Io)
}

async fn listener_loop(
    listener: interprocess::local_socket::tokio::Listener,
    service: hyphae_native_product::NativeProductHandle,
    config: NativeDaemonConfig,
    authentication: DaemonAuthentication,
    denied_client_identity: Option<String>,
    accepting: Arc<AtomicBool>,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<(), DaemonError> {
    use interprocess::local_socket::tokio::prelude::*;

    let mut clients = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.recv() => {
                break;
            }
            accepted = listener.accept(), if accepting.load(Ordering::Acquire) => {
                let stream = accepted?;
                if clients.len() >= config.maximum_clients {
                    drop(stream);
                    continue;
                }
                let service = service.clone();
                let client_shutdown = shutdown.resubscribe();
                let denied_client_identity = denied_client_identity.clone();
                clients.spawn(async move {
                    serve_connection(
                        stream,
                        service,
                        config,
                        authentication,
                        denied_client_identity,
                        client_shutdown,
                    )
                    .await
                });
            }
            completed = clients.join_next(), if !clients.is_empty() => {
                if let Some(completed) = completed {
                    handle_connection_completion(completed)?;
                }
            }
        }
    }
    drop(listener);
    while let Some(completed) = clients.join_next().await {
        handle_connection_completion(completed)?;
    }
    Ok(())
}

fn handle_connection_completion(
    completed: Result<Result<(), DaemonError>, tokio::task::JoinError>,
) -> Result<(), DaemonError> {
    match completed {
        Ok(Ok(())) | Err(_) => Ok(()),
        Ok(Err(error)) if connection_error_is_local(&error) => Ok(()),
        Ok(Err(error)) => Err(error),
        // Panics are isolated to the accepted client task. Listener accept
        // failure remains the only asynchronous transport failure that is fatal.
    }
}

fn connection_error_is_local(error: &DaemonError) -> bool {
    match error {
        DaemonError::InvalidPeerIdentity
        | DaemonError::ClientProtocol
        | DaemonError::Io(_)
        | DaemonError::Frame(_)
        | DaemonError::Handshake(_)
        | DaemonError::ProductCodec(_)
        | DaemonError::Control(_)
        | DaemonError::Task => true,
        DaemonError::Product(error) => !matches!(
            error.code(),
            ProductErrorCode::Internal | ProductErrorCode::Unavailable
        ),
        DaemonError::InvalidConfiguration | DaemonError::InvalidEndpoint => false,
    }
}

async fn serve_connection(
    stream: interprocess::local_socket::tokio::Stream,
    service: hyphae_native_product::NativeProductHandle,
    config: NativeDaemonConfig,
    authentication: DaemonAuthentication,
    denied_client_identity: Option<String>,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<(), DaemonError> {
    let stream = Arc::new(stream);
    let peer = PeerIdentity::from_credentials(stream.peer_creds()?);
    let mut codec = AsyncFrameIo::new(config.maximum_frame_payload)?;
    let mut reader = stream.as_ref();
    let first = tokio::select! {
        result = codec.receive(&mut reader) => result?,
        _ = shutdown.recv() => {
            return Ok(());
        }
    };
    let Some(mut first) = first else {
        return Ok(());
    };
    if first.kind != FrameKind::Hello || first.stream_id != 0 || first.request_id == 0 {
        first.payload.fill(0);
        return Err(DaemonError::Handshake(HandshakeError::Malformed));
    }
    let decoded = decode_connection_credential(
        authentication,
        denied_client_identity.as_deref(),
        peer,
        &first.payload,
    );
    first.payload.fill(0);
    let opened = open_connection_client(
        stream.as_ref(),
        &codec,
        &service,
        authentication,
        decoded,
        first.request_id,
    )
    .await;
    let Some((hello, client, catalog_version)) = opened? else {
        return Ok(());
    };
    let client = Arc::new(client);
    let policy = NegotiationPolicy {
        capabilities: match authentication {
            DaemonAuthentication::UnmanagedPeer => ProtocolCapabilities::G6,
            DaemonAuthentication::ManagedApiKey => ProtocolCapabilities::G6_AUTHENTICATED,
        },
        maximum_frame_payload: u32::try_from(config.maximum_frame_payload)
            .map_err(|_| DaemonError::InvalidConfiguration)?,
        maximum_in_flight: config.maximum_in_flight,
        maximum_initial_window: u32::try_from(config.maximum_window)
            .map_err(|_| DaemonError::InvalidConfiguration)?,
    };
    let welcome = negotiate(
        &hello,
        policy,
        client.session_id().get(),
        hyphae_native_product::capabilities(),
        catalog_version,
    )?;
    codec
        .send(
            &mut stream.as_ref(),
            FrameKind::Welcome,
            0,
            first.request_id,
            &encode_welcome(welcome)?,
        )
        .await?;

    let negotiated_payload = usize::try_from(welcome.maximum_frame_payload)
        .map_err(|_| DaemonError::InvalidConfiguration)?;
    let codec = AsyncFrameIo::new(negotiated_payload)?;

    let request_state = Arc::new(Mutex::new(BTreeMap::new()));
    let pending_controls = Arc::new(Mutex::new(BTreeMap::new()));
    let window = Arc::new(Mutex::new(BTreeMap::new()));
    let window_notify = Arc::new(Notify::new());
    let result = connection_loop(
        stream.clone(),
        &codec,
        client.clone(),
        welcome,
        config,
        request_state.clone(),
        pending_controls.clone(),
        window.clone(),
        window_notify,
        &mut shutdown,
    )
    .await;
    cleanup_connection_state(&request_state, &pending_controls, &window);
    if let Ok(client) = Arc::try_unwrap(client) {
        let _ignored = client.close();
    }
    result
}

async fn open_connection_client(
    stream: &interprocess::local_socket::tokio::Stream,
    codec: &AsyncFrameIo,
    service: &hyphae_native_product::NativeProductHandle,
    authentication: DaemonAuthentication,
    decoded: Result<(Hello, ConnectionCredential), DaemonError>,
    request_id: u64,
) -> Result<Option<(Hello, NativeProductClient, u64)>, DaemonError> {
    let (hello, credential) = match decoded {
        Ok(decoded) => decoded,
        Err(DaemonError::Product(error))
            if error.code() == ProductErrorCode::AuthorizationDenied =>
        {
            send_handshake_error(stream, codec, request_id, error).await?;
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let requested_namespace_is_supported = hello.database == "main" && hello.schema == "public";
    if !requested_namespace_is_supported && authentication == DaemonAuthentication::UnmanagedPeer {
        send_handshake_error(
            stream,
            codec,
            request_id,
            ProductError::from_code(ProductErrorCode::InvalidRequest),
        )
        .await?;
        return Ok(None);
    }
    let client = match credential {
        ConnectionCredential::Unmanaged {
            principal,
            authorization,
        } => service.open_session(principal, authorization)?,
        ConnectionCredential::Managed(credential) => {
            match service.open_authenticated_session(credential) {
                Ok(client) => client,
                Err(error) if error.code() == ProductErrorCode::AuthorizationDenied => {
                    send_handshake_error(stream, codec, request_id, error).await?;
                    return Ok(None);
                }
                Err(error) => return Err(error.into()),
            }
        }
    };
    if !requested_namespace_is_supported {
        let _ignored = client.close();
        send_handshake_error(
            stream,
            codec,
            request_id,
            ProductError::from_code(ProductErrorCode::InvalidRequest),
        )
        .await?;
        return Ok(None);
    }
    let catalog_version = match authentication {
        DaemonAuthentication::ManagedApiKey => 0,
        DaemonAuthentication::UnmanagedPeer => match client.dispatch(
            client.request_context(u128::from(request_id), 0),
            ProductOperation::AdminStatus,
        )? {
            ProductResponse::AdminStatus(status) => status.snapshot.catalog_version.get(),
            _ => return Err(DaemonError::Task),
        },
    };
    Ok(Some((hello, client, catalog_version)))
}

fn decode_connection_credential(
    authentication: DaemonAuthentication,
    denied_client_identity: Option<&str>,
    peer: PeerIdentity,
    payload: &[u8],
) -> Result<(Hello, ConnectionCredential), DaemonError> {
    match authentication {
        DaemonAuthentication::UnmanagedPeer => {
            let hello = decode_hello(payload)?;
            let authorization = if denied_client_identity == Some(hello.client_identity.as_str()) {
                ProductAuthorization::from_permissions([ProductPermission::Observe])
            } else {
                ProductAuthorization::ALL
            };
            let principal = peer.principal(&hello)?;
            Ok((
                hello,
                ConnectionCredential::Unmanaged {
                    principal,
                    authorization,
                },
            ))
        }
        DaemonAuthentication::ManagedApiKey => {
            let authenticated = decode_authenticated_hello(payload).map_err(|_| {
                DaemonError::Product(ProductError::from_code(
                    ProductErrorCode::AuthorizationDenied,
                ))
            })?;
            let (hello, credential) = authenticated.into_parts();
            Ok((hello, ConnectionCredential::Managed(credential)))
        }
    }
}

async fn send_handshake_error(
    stream: &interprocess::local_socket::tokio::Stream,
    codec: &AsyncFrameIo,
    request_id: u64,
    error: ProductError,
) -> Result<(), DaemonError> {
    send_product_error(stream, codec, &AsyncMutex::new(()), 0, request_id, error).await
}

#[derive(Clone)]
struct ActiveRequest {
    stream_id: u32,
    generation: u64,
    cancellation: hyphae_native_product::ProductCancellationToken,
}

#[derive(Clone, Copy)]
struct PendingControl {
    stream_id: u32,
    deadline_micros: Option<i64>,
    cancelled: bool,
}

struct ActiveWindow {
    request_id: u64,
    generation: u64,
    window: FlowWindow,
}

type RequestState = Arc<Mutex<BTreeMap<u64, ActiveRequest>>>;
type PendingControls = Arc<Mutex<BTreeMap<u64, PendingControl>>>;
type WindowState = Arc<Mutex<BTreeMap<u32, ActiveWindow>>>;

#[allow(
    clippy::manual_let_else,
    clippy::map_entry,
    clippy::single_match_else,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]
async fn connection_loop(
    stream: Arc<interprocess::local_socket::tokio::Stream>,
    codec: &AsyncFrameIo,
    client: Arc<NativeProductClient>,
    welcome: hyphae_native_protocol::Welcome,
    config: NativeDaemonConfig,
    requests: RequestState,
    pending_controls: PendingControls,
    windows: WindowState,
    window_notify: Arc<Notify>,
    shutdown: &mut broadcast::Receiver<()>,
) -> Result<(), DaemonError> {
    let negotiated_payload = usize::try_from(welcome.maximum_frame_payload)
        .map_err(|_| DaemonError::InvalidConfiguration)?;
    let mut receive_codec = AsyncFrameIo::new(negotiated_payload)?;
    let mut reader = stream.as_ref();
    let output = Arc::new(AsyncMutex::new(()));
    let mut responses = JoinSet::new();
    let mut next_generation = 1_u64;
    loop {
        let received: Option<hyphae_native_protocol::OwnedFrame> = tokio::select! {
            result = receive_codec.receive(&mut reader) => result?,
            _ = shutdown.recv() => {
                break;
            }
            completed = responses.join_next(), if !responses.is_empty() => {
                match completed {
                    Some(Ok(Ok(()) | Err(_)) | Err(_)) | None => continue,
                }
            }
        };
        let Some(frame) = received else {
            break;
        };
        match frame.kind {
            FrameKind::Ping => {
                let _output = output.lock().await;
                codec
                    .send(
                        &mut stream.as_ref(),
                        FrameKind::Ping,
                        frame.stream_id,
                        frame.request_id,
                        &frame.payload,
                    )
                    .await?;
            }
            FrameKind::Deadline => {
                validate_control_identity(frame.stream_id, frame.request_id)?;
                let deadline_micros = decode_deadline(&frame.payload)?;
                if requests
                    .lock()
                    .map_err(|_| DaemonError::Task)?
                    .contains_key(&frame.request_id)
                    || windows
                        .lock()
                        .map_err(|_| DaemonError::Task)?
                        .contains_key(&frame.stream_id)
                {
                    return Err(DaemonError::ClientProtocol);
                }
                let maximum = maximum_in_flight(welcome)?;
                let active = requests.lock().map_err(|_| DaemonError::Task)?.len();
                let at_capacity = {
                    let pending = pending_controls.lock().map_err(|_| DaemonError::Task)?;
                    !pending.contains_key(&frame.request_id)
                        && pending.len().saturating_add(active) >= maximum
                };
                if at_capacity {
                    send_product_error(
                        stream.as_ref(),
                        codec,
                        &output,
                        frame.stream_id,
                        frame.request_id,
                        ProductError::from_code(ProductErrorCode::Unavailable),
                    )
                    .await?;
                    continue;
                }
                let mut pending = pending_controls.lock().map_err(|_| DaemonError::Task)?;
                if pending.iter().any(|(request_id, control)| {
                    *request_id != frame.request_id && control.stream_id == frame.stream_id
                }) {
                    return Err(DaemonError::ClientProtocol);
                }
                let control = pending.entry(frame.request_id).or_insert(PendingControl {
                    stream_id: frame.stream_id,
                    deadline_micros: None,
                    cancelled: false,
                });
                if control.stream_id != frame.stream_id {
                    return Err(DaemonError::ClientProtocol);
                }
                control.deadline_micros = Some(deadline_micros);
            }
            FrameKind::Cancel => {
                let _reason = decode_cancel(&frame.payload)?;
                validate_control_identity(frame.stream_id, frame.request_id)?;
                let active = requests
                    .lock()
                    .map_err(|_| DaemonError::Task)?
                    .get(&frame.request_id)
                    .cloned();
                if let Some(active) = active {
                    if active.stream_id != frame.stream_id {
                        return Err(DaemonError::ClientProtocol);
                    }
                    active.cancellation.cancel();
                } else {
                    if windows
                        .lock()
                        .map_err(|_| DaemonError::Task)?
                        .contains_key(&frame.stream_id)
                    {
                        return Err(DaemonError::ClientProtocol);
                    }
                    let maximum = maximum_in_flight(welcome)?;
                    let active = requests.lock().map_err(|_| DaemonError::Task)?.len();
                    let at_capacity = {
                        let pending = pending_controls.lock().map_err(|_| DaemonError::Task)?;
                        !pending.contains_key(&frame.request_id)
                            && pending.len().saturating_add(active) >= maximum
                    };
                    if at_capacity {
                        send_product_error(
                            stream.as_ref(),
                            codec,
                            &output,
                            frame.stream_id,
                            frame.request_id,
                            ProductError::from_code(ProductErrorCode::Unavailable),
                        )
                        .await?;
                        continue;
                    }
                    let mut pending = pending_controls.lock().map_err(|_| DaemonError::Task)?;
                    if pending.iter().any(|(request_id, control)| {
                        *request_id != frame.request_id && control.stream_id == frame.stream_id
                    }) {
                        return Err(DaemonError::ClientProtocol);
                    }
                    let control = pending.entry(frame.request_id).or_insert(PendingControl {
                        stream_id: frame.stream_id,
                        deadline_micros: None,
                        cancelled: false,
                    });
                    if control.stream_id != frame.stream_id {
                        return Err(DaemonError::ClientProtocol);
                    }
                    control.cancelled = true;
                }
            }
            FrameKind::WindowUpdate => {
                validate_control_identity(frame.stream_id, frame.request_id)?;
                let increment = decode_window_update(&frame.payload)?;
                let mut windows = windows.lock().map_err(|_| DaemonError::Task)?;
                let window = windows
                    .get_mut(&frame.stream_id)
                    .ok_or(DaemonError::ClientProtocol)?;
                if window.request_id != frame.request_id {
                    return Err(DaemonError::ClientProtocol);
                }
                window.window.update(increment)?;
                drop(windows);
                window_notify.notify_waiters();
            }
            FrameKind::Execute | FrameKind::Prepare | FrameKind::Deallocate => {
                validate_control_identity(frame.stream_id, frame.request_id)?;
                let duplicate_active = requests
                    .lock()
                    .map_err(|_| DaemonError::Task)?
                    .contains_key(&frame.request_id);
                let duplicate_stream = windows
                    .lock()
                    .map_err(|_| DaemonError::Task)?
                    .contains_key(&frame.stream_id);
                let duplicate_pending_stream = pending_controls
                    .lock()
                    .map_err(|_| DaemonError::Task)?
                    .iter()
                    .any(|(request_id, control)| {
                        *request_id != frame.request_id && control.stream_id == frame.stream_id
                    });
                if duplicate_active || duplicate_stream || duplicate_pending_stream {
                    return Err(DaemonError::ClientProtocol);
                }
                let active = requests.lock().map_err(|_| DaemonError::Task)?.len();
                let (pending_count, has_pending) = {
                    let pending = pending_controls.lock().map_err(|_| DaemonError::Task)?;
                    (pending.len(), pending.contains_key(&frame.request_id))
                };
                if active >= maximum_in_flight(welcome)?
                    || (!has_pending
                        && active.saturating_add(pending_count) >= maximum_in_flight(welcome)?)
                {
                    pending_controls
                        .lock()
                        .map_err(|_| DaemonError::Task)?
                        .remove(&frame.request_id);
                    send_product_error(
                        stream.as_ref(),
                        codec,
                        &output,
                        frame.stream_id,
                        frame.request_id,
                        ProductError::from_code(ProductErrorCode::Unavailable),
                    )
                    .await?;
                    continue;
                }
                let decode_started = Instant::now();
                let request = match decode_product_request(&frame.payload) {
                    Ok(request) => request,
                    Err(_) => {
                        pending_controls
                            .lock()
                            .map_err(|_| DaemonError::Task)?
                            .remove(&frame.request_id);
                        send_product_error(
                            stream.as_ref(),
                            codec,
                            &output,
                            frame.stream_id,
                            frame.request_id,
                            ProductError::from_code(ProductErrorCode::InvalidRequest),
                        )
                        .await?;
                        continue;
                    }
                };
                client.record_timing(TimingClass::RequestDecoding, decode_started.elapsed());
                if !frame_accepts_operation(frame.kind, &request.operation) {
                    pending_controls
                        .lock()
                        .map_err(|_| DaemonError::Task)?
                        .remove(&frame.request_id);
                    send_product_error(
                        stream.as_ref(),
                        codec,
                        &output,
                        frame.stream_id,
                        frame.request_id,
                        ProductError::from_code(ProductErrorCode::InvalidRequest),
                    )
                    .await?;
                    continue;
                }
                let token = hyphae_native_product::ProductCancellationToken::new();
                let pending_control = pending_controls
                    .lock()
                    .map_err(|_| DaemonError::Task)?
                    .remove(&frame.request_id);
                if pending_control.is_some_and(|control| control.stream_id != frame.stream_id) {
                    return Err(DaemonError::ClientProtocol);
                }
                if pending_control.is_some_and(|control| control.cancelled) {
                    token.cancel();
                }
                let generation = next_generation;
                next_generation = next_generation
                    .checked_add(1)
                    .ok_or(DaemonError::ClientProtocol)?;
                {
                    // Admission shares the output lock with terminal frames so a
                    // completed stream cannot be reused until its END or FAILURE
                    // is visible on the connection.
                    let _output = output.lock().await;
                    let mut active = requests.lock().map_err(|_| DaemonError::Task)?;
                    let mut active_windows = windows.lock().map_err(|_| DaemonError::Task)?;
                    if active.contains_key(&frame.request_id)
                        || active_windows.contains_key(&frame.stream_id)
                    {
                        return Err(DaemonError::ClientProtocol);
                    }
                    active.insert(
                        frame.request_id,
                        ActiveRequest {
                            stream_id: frame.stream_id,
                            generation,
                            cancellation: token.clone(),
                        },
                    );
                    active_windows.insert(
                        frame.stream_id,
                        ActiveWindow {
                            request_id: frame.request_id,
                            generation,
                            window: FlowWindow::new(
                                u64::from(welcome.initial_window),
                                config.maximum_window,
                            )?,
                        },
                    );
                }
                let mut context = client
                    .request_context(u128::from(frame.request_id), request.logical_time_micros);
                context.deadline_micros = request
                    .deadline_micros
                    .or_else(|| pending_control.and_then(|control| control.deadline_micros));
                context.cancellation = token;
                context.idempotency_token = request.idempotency_token;
                context.limits = request.limits;
                context.durability = request.durability;
                let pending = match client.submit_async(context, request.operation) {
                    Ok(pending) => pending,
                    Err(error) => {
                        send_terminal_product_error(
                            stream.as_ref(),
                            codec,
                            &output,
                            &requests,
                            &windows,
                            frame.stream_id,
                            frame.request_id,
                            generation,
                            error,
                        )
                        .await?;
                        continue;
                    }
                };
                let response_requests = requests.clone();
                let response_windows = windows.clone();
                let response_notify = window_notify.clone();
                let response_output = output.clone();
                let response_stream = stream.clone();
                let response_client = client.clone();
                let mut response_shutdown = shutdown.resubscribe();
                responses.spawn(async move {
                    let response = pending.wait().await;
                    let sent = match AsyncFrameIo::new(negotiated_payload) {
                        Ok(response_codec) => match response {
                            Ok(response) => {
                                let encoding_started = Instant::now();
                                match encode_product_response(&response) {
                                    Ok(encoded) => {
                                        response_client.record_timing(
                                            TimingClass::ResultEncoding,
                                            encoding_started.elapsed(),
                                        );
                                        let transport_started = Instant::now();
                                        send_stream(
                                            response_stream.as_ref(),
                                            &response_codec,
                                            &response_output,
                                            frame.stream_id,
                                            frame.request_id,
                                            generation,
                                            &encoded,
                                            &response_requests,
                                            &response_windows,
                                            &response_notify,
                                            &mut response_shutdown,
                                        )
                                        .await
                                        .inspect(|()| {
                                            response_client.record_timing(
                                                TimingClass::Transport,
                                                transport_started.elapsed(),
                                            );
                                        })
                                    }
                                    Err(error) => Err(error.into()),
                                }
                            }
                            Err(error) => {
                                send_terminal_product_error(
                                    response_stream.as_ref(),
                                    &response_codec,
                                    &response_output,
                                    &response_requests,
                                    &response_windows,
                                    frame.stream_id,
                                    frame.request_id,
                                    generation,
                                    error,
                                )
                                .await
                            }
                        },
                        Err(error) => Err(error.into()),
                    };
                    finish_request(
                        &response_requests,
                        &response_windows,
                        frame.stream_id,
                        frame.request_id,
                        generation,
                    )?;
                    sent
                });
            }
            FrameKind::Close => {
                if frame.stream_id != 0 || !frame.payload.is_empty() {
                    return Err(DaemonError::ClientProtocol);
                }
                let _output = output.lock().await;
                codec
                    .send(
                        &mut stream.as_ref(),
                        FrameKind::Close,
                        0,
                        frame.request_id,
                        &[],
                    )
                    .await?;
                break;
            }
            _ => return Err(DaemonError::ClientProtocol),
        }
    }
    for request in requests.lock().map_err(|_| DaemonError::Task)?.values() {
        request.cancellation.cancel();
    }
    responses.abort_all();
    while responses.join_next().await.is_some() {}
    Ok(())
}

fn maximum_in_flight(welcome: hyphae_native_protocol::Welcome) -> Result<usize, DaemonError> {
    usize::try_from(welcome.maximum_in_flight).map_err(|_| DaemonError::InvalidConfiguration)
}

fn validate_control_identity(stream_id: u32, request_id: u64) -> Result<(), DaemonError> {
    if stream_id == 0 || request_id == 0 {
        Err(DaemonError::ClientProtocol)
    } else {
        Ok(())
    }
}

fn finish_request(
    requests: &RequestState,
    windows: &WindowState,
    stream_id: u32,
    request_id: u64,
    generation: u64,
) -> Result<(), DaemonError> {
    let mut requests = requests.lock().map_err(|_| DaemonError::Task)?;
    if requests
        .get(&request_id)
        .is_some_and(|request| request.stream_id == stream_id && request.generation == generation)
    {
        requests.remove(&request_id);
    }
    let mut windows = windows.lock().map_err(|_| DaemonError::Task)?;
    if windows
        .get(&stream_id)
        .is_some_and(|window| window.request_id == request_id && window.generation == generation)
    {
        windows.remove(&stream_id);
    }
    Ok(())
}

fn cleanup_connection_state(
    requests: &RequestState,
    pending_controls: &PendingControls,
    windows: &WindowState,
) {
    if let Ok(mut requests) = requests.lock() {
        for request in requests.values() {
            request.cancellation.cancel();
        }
        requests.clear();
    }
    if let Ok(mut pending_controls) = pending_controls.lock() {
        pending_controls.clear();
    }
    if let Ok(mut windows) = windows.lock() {
        windows.clear();
    }
}

fn frame_accepts_operation(kind: FrameKind, operation: &ProductOperation) -> bool {
    matches!(
        (kind, operation),
        (FrameKind::Prepare, ProductOperation::PrepareSql { .. })
            | (
                FrameKind::Deallocate,
                ProductOperation::DeallocatePrepared { .. }
            )
    ) || (kind == FrameKind::Execute
        && !matches!(
            operation,
            ProductOperation::PrepareSql { .. } | ProductOperation::DeallocatePrepared { .. }
        ))
}

#[allow(clippy::too_many_arguments)]
async fn send_stream(
    stream: &interprocess::local_socket::tokio::Stream,
    codec: &AsyncFrameIo,
    output: &AsyncMutex<()>,
    stream_id: u32,
    request_id: u64,
    generation: u64,
    response: &[u8],
    requests: &RequestState,
    windows: &WindowState,
    notify: &Notify,
    shutdown: &mut broadcast::Receiver<()>,
) -> Result<(), DaemonError> {
    let mut offset = 0;
    while offset < response.len() {
        let chunk = {
            let mut windows = windows.lock().map_err(|_| DaemonError::Task)?;
            let window = windows.get_mut(&stream_id).ok_or(DaemonError::Task)?;
            if window.request_id != request_id || window.generation != generation {
                return Err(DaemonError::Task);
            }
            let available = usize::try_from(window.window.available()).unwrap_or(usize::MAX);
            let chunk = response
                .len()
                .saturating_sub(offset)
                .min(available)
                .min(codec.maximum_payload());
            if chunk > 0 {
                window.window.consume(chunk)?;
            }
            chunk
        };
        if chunk == 0 {
            tokio::select! {
                () = notify.notified() => {}
                _ = shutdown.recv() => {
                    return Ok(());
                }
            }
            continue;
        }
        let _output = output.lock().await;
        codec
            .send(
                &mut &*stream,
                FrameKind::Data,
                stream_id,
                request_id,
                &response[offset..offset + chunk],
            )
            .await?;
        offset += chunk;
    }
    let completion = StreamCompletion::for_data(response)?;
    let _output = output.lock().await;
    finish_request(requests, windows, stream_id, request_id, generation)?;
    codec
        .send(
            &mut &*stream,
            FrameKind::End,
            stream_id,
            request_id,
            &encode_end(completion),
        )
        .await?;
    Ok(())
}

async fn send_product_error(
    stream: &interprocess::local_socket::tokio::Stream,
    codec: &AsyncFrameIo,
    output: &AsyncMutex<()>,
    stream_id: u32,
    request_id: u64,
    error: ProductError,
) -> Result<(), DaemonError> {
    let encoded = encode_product_failure(codec, request_id, error)?;
    let _output = output.lock().await;
    send_product_failure(stream, codec, stream_id, request_id, &encoded).await
}

#[allow(clippy::too_many_arguments)]
async fn send_terminal_product_error(
    stream: &interprocess::local_socket::tokio::Stream,
    codec: &AsyncFrameIo,
    output: &AsyncMutex<()>,
    requests: &RequestState,
    windows: &WindowState,
    stream_id: u32,
    request_id: u64,
    generation: u64,
    error: ProductError,
) -> Result<(), DaemonError> {
    let encoded = encode_product_failure(codec, request_id, error)?;
    let _output = output.lock().await;
    finish_request(requests, windows, stream_id, request_id, generation)?;
    send_product_failure(stream, codec, stream_id, request_id, &encoded).await
}

fn encode_product_failure(
    codec: &AsyncFrameIo,
    request_id: u64,
    error: ProductError,
) -> Result<Vec<u8>, DaemonError> {
    let error = if error.request_id().is_some() {
        error
    } else {
        error.with_request_id(u128::from(request_id))
    };
    let encoded = encode_failure(&error)?;
    if encoded.len() > codec.maximum_payload() {
        return Err(DaemonError::ClientProtocol);
    }
    Ok(encoded)
}

async fn send_product_failure(
    stream: &interprocess::local_socket::tokio::Stream,
    codec: &AsyncFrameIo,
    stream_id: u32,
    request_id: u64,
    encoded: &[u8],
) -> Result<(), DaemonError> {
    codec
        .send(
            &mut &*stream,
            FrameKind::Failure,
            stream_id,
            request_id,
            encoded,
        )
        .await?;
    Ok(())
}

#[cfg(all(test, windows))]
mod windows_tests {
    use interprocess::os::windows::security_descriptor::AsSecurityDescriptorExt as _;

    use super::*;

    #[test]
    fn named_pipe_dacl_is_protected_and_owner_system_only() -> Result<(), DaemonError> {
        const DACL_SECURITY_INFORMATION: u32 = 4;

        let descriptor = windows_security_descriptor()?;
        let sddl = descriptor.serialize(
            DACL_SECURITY_INFORMATION,
            widestring::U16CStr::to_string_lossy,
        )?;
        assert!(sddl.contains("D:P"));
        assert!(sddl.contains("(A;;GA;;;OW)"));
        assert!(sddl.contains("(A;;GA;;;SY)"));
        assert!(!sddl.contains(";;;WD)"));
        assert!(!sddl.contains(";;;AU)"));
        Ok(())
    }
}
