// SPDX-License-Identifier: Apache-2.0

//! Native HTTP v2 edge adapter over the one-owner product service.

mod config;
mod error;
mod server;

pub use config::{NativeHttpV2Config, NativeHttpV2ConfigError, NativeHttpV2Limits};
pub use error::NativeHttpV2Error;
pub use server::{BoundNativeHttpV2Server, NativeHttpV2Server};

pub(crate) const PRODUCT_MEDIA_TYPE: &str = hyphae_contracts::v2::PRODUCT_MEDIA_TYPE_V2;
pub(crate) const ERROR_MEDIA_TYPE: &str = hyphae_contracts::v2::PRODUCT_ERROR_MEDIA_TYPE_V2;
pub(crate) const REQUEST_ID_HEADER: &str = hyphae_contracts::v2::REQUEST_ID_HEADER_V2;

/// Native-service compatibility policy for the published HTTP v1 surface.
///
/// The original [`crate::HyphaeServer`] retains its current format-2 `/v1`
/// behavior. The Native HTTP server owns no format-2 engine and rejects every
/// v1 operation for which no exact native result and proof mapping exists.
pub const NATIVE_V1_COMPATIBILITY_POLICY: &str =
    "exact mappings only; unmappable operations fail; format-2 state is never opened";

use error::NativeApiError;
use server::RequestMetadata;

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use std::{
        error::Error,
        fs,
        net::Ipv4Addr,
        path::PathBuf,
        sync::{
            Arc, Barrier,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
        time::Duration,
    };

    use axum::{
        body::{self, Body, Bytes},
        http::{HeaderMap, Request, StatusCode, header},
    };
    use hyphae_native_product::{
        ApiKeyId, BuiltInRole, NativeProduct, NativeProductService, NativeProductServiceConfig,
        ProductAuthorization, ProductDurabilityPolicy, ProductLimits, ProductOperation,
        ProductResponse, ProductScope,
    };
    use hyphae_native_protocol::{WireRequest, decode_product_response, encode_product_request};
    use serde_json::Value;
    use tokio::sync::watch;
    use tower::ServiceExt as _;

    use super::{
        ERROR_MEDIA_TYPE, NativeHttpV2Config, NativeHttpV2ConfigError, NativeHttpV2Limits,
        NativeHttpV2Server, PRODUCT_MEDIA_TYPE, REQUEST_ID_HEADER,
    };
    use crate::BearerToken;

    struct TestDirectory(PathBuf);

    struct ManagedServiceFixture {
        service: NativeProductService,
        credential: String,
        owner_credential: String,
        credential_key_id: ApiKeyId,
        _directory: TestDirectory,
    }

    #[test]
    fn openapi_contract_lists_every_native_http_v2_route() {
        let contract = hyphae_contracts::OPENAPI_V2;
        for route in [
            "/v2/capabilities",
            "/v2/execute",
            "/v2/catalog",
            "/v2/sql",
            "/v2/structures",
            "/v2/search",
            "/v2/search/collection",
            "/v2/search/ingest",
            "/v2/search/document",
            "/v2/admin",
            "/v2/telemetry",
            "/v2/doctor",
            "/v2/backup",
            "/v2/restore",
            "/v2/proofs/verify",
            "/v2/transactions/status",
            "/v2/security/keys",
            "/v2/read-stream",
        ] {
            assert!(contract.contains(&format!("  {route}:")), "missing {route}");
        }
        assert!(
            !contract.contains("Explicitly unsupported until ProductOperation exposes restore")
        );
        assert!(!contract.contains("\"501\""));
        assert!(contract.contains("x-hyphae-authentication-modes:"));
        assert!(contract.contains("x-hyphae-protocol-minor:"));
        assert_eq!(
            contract
                .matches(
                    "X-Hyphae-Protocol-Minor: { $ref: \"#/components/headers/ProtocolMinor\" }"
                )
                .count(),
            6
        );
        assert_eq!(
            contract
                .matches("parameters: [{ $ref: \"#/components/parameters/ProtocolMinor\" }]")
                .count(),
            18
        );
        assert!(contract.contains("description: Exact Native product protocol minor"));
        assert!(contract.contains("Generic /v2/execute rejects these variants"));
        assert!(contract.contains("legacy-migration-1.2:"));
        assert!(contract.contains("Canonical hyp1 is always parsed without fallback"));
        assert!(contract.contains("After bootstrap, new automatically requires one canonical"));
        assert!(contract.contains("adapter is loopback-only; remote exposure requires a TLS"));
        assert!(
            contract
                .contains("bearerFormat: hyp1_<32-lowercase-hex-key-id>_<64-lowercase-hex-secret>")
        );
        assert!(contract.contains("authority loss is HTTP 403"));
        assert!(contract.contains("Revoked canonical candidates retain only opaque pending state"));
        assert!(contract.contains("bounded to 256 active tasks"));
        assert_eq!(
            contract
                .matches("\"401\": { $ref: \"#/components/responses/AuthenticationRequired\" }")
                .count(),
            18
        );
        assert_eq!(
            contract
                .matches("\"403\": { $ref: \"#/components/responses/AuthorizationDenied\" }")
                .count(),
            18
        );
    }

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "hyphae-native-http-v2-{name}-{}-{}",
                std::process::id(),
                uuid::Uuid::now_v7()
            ));
            let _ignored = fs::remove_dir_all(&path);
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ignored = fs::remove_dir_all(&self.0);
        }
    }

    fn service(name: &str) -> Result<(TestDirectory, NativeProductService), Box<dyn Error>> {
        let directory = TestDirectory::new(name);
        let product = NativeProduct::create(&directory.0)?;
        let service = NativeProductService::start(product, NativeProductServiceConfig::default())?;
        Ok((directory, service))
    }

    fn managed_service(name: &str, reader: bool) -> Result<ManagedServiceFixture, Box<dyn Error>> {
        let directory = TestDirectory::new(name);
        let data_path = directory.0.join("data");
        let owner_key_path = directory.0.join("owner.key");
        let reader_key_path = directory.0.join("reader.key");
        fs::create_dir_all(&directory.0)?;
        let mut product = NativeProduct::create(data_path)?;
        product.bootstrap_access_control_to_file("Owner", "owner", &owner_key_path, 1)?;
        let owner_secret = fs::read_to_string(owner_key_path)?;
        let (credential, credential_key_id) = if reader {
            let owner = product.authenticate_api_key(&owner_secret, 2)?;
            let principal = product.create_security_principal(&owner, "HTTP reader", 2)?;
            let owner = product.authenticate_api_key(&owner_secret, 3)?;
            product.assign_built_in_role(
                &owner,
                principal.principal_id,
                BuiltInRole::Reader,
                ProductScope::Instance,
                3,
            )?;
            let owner = product.authenticate_api_key(&owner_secret, 4)?;
            product.set_security_principal_enabled(&owner, principal.principal_id, true, 4)?;
            let owner = product.authenticate_api_key(&owner_secret, 5)?;
            let issued = product.issue_api_key_to_file(
                &owner,
                principal.principal_id,
                "http-reader",
                [BuiltInRole::Reader],
                BuiltInRole::Reader.authorization(),
                None,
                &reader_key_path,
                5,
            )?;
            (fs::read_to_string(reader_key_path)?, issued.key_id)
        } else {
            let owner = product.authenticate_api_key(&owner_secret, 2)?;
            (owner_secret.clone(), owner.key_id())
        };
        let service = NativeProductService::start(product, NativeProductServiceConfig::default())?;
        Ok(ManagedServiceFixture {
            service,
            credential,
            owner_credential: owner_secret,
            credential_key_id,
            _directory: directory,
        })
    }

    fn request(operation: ProductOperation) -> Result<Vec<u8>, Box<dyn Error>> {
        Ok(encode_product_request(&WireRequest {
            operation,
            logical_time_micros: 1,
            deadline_micros: None,
            idempotency_token: None,
            limits: ProductLimits::default(),
            durability: ProductDurabilityPolicy::STRICT,
        })?)
    }

    fn mutation_request(
        operation: ProductOperation,
        idempotency_token: u128,
    ) -> Result<Vec<u8>, Box<dyn Error>> {
        Ok(encode_product_request(&WireRequest {
            operation,
            logical_time_micros: 1,
            deadline_micros: None,
            idempotency_token: Some(idempotency_token),
            limits: ProductLimits::default(),
            durability: ProductDurabilityPolicy::STRICT,
        })?)
    }

    #[tokio::test]
    async fn api_key_start_is_cache_safe_and_never_compressed() -> Result<(), Box<dyn Error>> {
        let fixture = managed_service("key-start-headers", false)?;
        let owner = fixture.owner_credential.clone();
        let actor = fixture
            .service
            .handle()
            .open_authenticated_session(hyphae_native_product::ApiKeyCredential::new(&owner)?)?;
        let principal_id: hyphae_native_product::SecurityId =
            actor.request_context(1, 1).principal.identity().parse()?;
        drop(actor);
        let app = NativeHttpV2Server::new_managed(
            fixture.service.handle(),
            NativeHttpV2Config::default(),
        )?
        .test_router();
        let response = app
            .oneshot(http_request(
                "/v2/security/keys",
                mutation_request(
                    ProductOperation::SecurityApiKeyIssueSelfStart {
                        principal_id,
                        label: "http-pending".to_owned(),
                        roles: vec![BuiltInRole::Owner],
                        custom_roles: Vec::new(),
                        permission_ceiling: ProductAuthorization::ALL,
                        scope_ceiling: vec![ProductScope::Instance],
                        expires_at_micros: None,
                    },
                    77,
                )?,
                Some("77"),
                Some(&owner),
                None,
            )?)
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "no-store, private, max-age=0"
        );
        assert_eq!(response.headers()[header::PRAGMA], "no-cache");
        assert!(!response.headers().contains_key(header::CONTENT_ENCODING));
        let _ = response_bytes(response).await?;
        Ok(())
    }

    fn http_request(
        path: &str,
        body: Vec<u8>,
        request_id: Option<&str>,
        token: Option<&str>,
        accept: Option<&str>,
    ) -> Result<Request<Body>, Box<dyn Error>> {
        let mut builder = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::CONTENT_TYPE, PRODUCT_MEDIA_TYPE)
            .header(hyphae_contracts::v2::PROTOCOL_MINOR_HEADER_V2, "3");
        if let Some(request_id) = request_id {
            builder = builder.header(REQUEST_ID_HEADER, request_id);
        }
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        if let Some(accept) = accept {
            builder = builder.header(header::ACCEPT, accept);
        }
        Ok(builder.body(Body::from(body))?)
    }

    fn http_request_with_session(
        path: &str,
        body: Vec<u8>,
        request_id: &str,
        token: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<Request<Body>, Box<dyn Error>> {
        let mut request = http_request(path, body, Some(request_id), token, None)?;
        if let Some(session_id) = session_id {
            request.headers_mut().insert(
                hyphae_contracts::v2::SESSION_ID_HEADER_V2,
                session_id.parse()?,
            );
        }
        Ok(request)
    }

    async fn response_bytes(response: axum::response::Response) -> Result<Bytes, Box<dyn Error>> {
        Ok(body::to_bytes(response.into_body(), 32 * 1024 * 1024).await?)
    }

    async fn response_snapshot(
        response: axum::response::Response,
    ) -> Result<(StatusCode, HeaderMap, Bytes), Box<dyn Error>> {
        let (parts, body) = response.into_parts();
        let bytes = body::to_bytes(body, 32 * 1024 * 1024).await?;
        Ok((parts.status, parts.headers, bytes))
    }

    struct RetainedSlowBody {
        retained: Arc<AtomicUsize>,
        maximum: Arc<AtomicUsize>,
        released: futures_util::future::BoxFuture<'static, ()>,
        entered: bool,
    }

    impl RetainedSlowBody {
        fn new(
            retained: Arc<AtomicUsize>,
            maximum: Arc<AtomicUsize>,
            mut release: watch::Receiver<bool>,
        ) -> Self {
            Self {
                retained,
                maximum,
                released: Box::pin(async move {
                    if !*release.borrow() {
                        let _changed = release.changed().await;
                    }
                }),
                entered: false,
            }
        }
    }

    impl Drop for RetainedSlowBody {
        fn drop(&mut self) {
            if self.entered {
                self.retained.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    impl futures_util::Stream for RetainedSlowBody {
        type Item = Result<Bytes, std::io::Error>;

        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            if !self.entered {
                let current = self.retained.fetch_add(1, Ordering::SeqCst) + 1;
                self.maximum.fetch_max(current, Ordering::SeqCst);
                self.entered = true;
            }
            match std::future::Future::poll(self.released.as_mut(), context) {
                std::task::Poll::Ready(()) => std::task::Poll::Ready(None),
                std::task::Poll::Pending => std::task::Poll::Pending,
            }
        }
    }

    #[tokio::test]
    async fn slow_bodies_are_bounded_before_authentication_and_buffering()
    -> Result<(), Box<dyn Error>> {
        let fixture = managed_service("bounded-body-readers", false)?;
        let server = NativeHttpV2Server::new_managed(
            fixture.service.handle(),
            NativeHttpV2Config {
                limits: NativeHttpV2Limits {
                    request_body_timeout: Duration::from_secs(30),
                    ..NativeHttpV2Limits::default()
                },
                ..NativeHttpV2Config::default()
            },
        )?;
        let app = server.test_router();
        let retained = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let (release, released) = watch::channel(false);
        let mut tasks = Vec::new();
        for request_id in 1..=super::server::MAX_HTTP_BODY_READERS + 32 {
            let stream = RetainedSlowBody::new(
                Arc::clone(&retained),
                Arc::clone(&maximum),
                released.clone(),
            );
            let request = Request::builder()
                .method("POST")
                .uri("/v2/execute")
                .header(header::CONTENT_TYPE, PRODUCT_MEDIA_TYPE)
                .header(hyphae_contracts::v2::PROTOCOL_MINOR_HEADER_V2, "3")
                .header(REQUEST_ID_HEADER, request_id.to_string())
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", fixture.credential),
                )
                .body(Body::from_stream(stream))?;
            let app = app.clone();
            tasks.push(tokio::spawn(async move { app.oneshot(request).await }));
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            while server.state.body_slots.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert_eq!(
            server.state.body_slots.available_permits(),
            0,
            "all bounded body-reader slots should be retained"
        );
        assert_eq!(
            maximum.load(Ordering::SeqCst),
            retained.load(Ordering::SeqCst),
            "no body reader may bypass the retained admission set"
        );
        assert!(maximum.load(Ordering::SeqCst) <= super::server::MAX_HTTP_BODY_READERS);
        release.send(true)?;
        let mut unavailable = 0;
        for task in tasks {
            if task.await??.status() == StatusCode::SERVICE_UNAVAILABLE {
                unavailable += 1;
            }
        }
        assert_eq!(unavailable, 32);
        drop(fixture.service);
        Ok(())
    }

    #[test]
    fn get_capability_authentication_is_bounded_before_blocking_work() -> Result<(), Box<dyn Error>>
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()?;
        runtime.block_on(async {
            let fixture = managed_service("bounded-get-auth", false)?;
            let server = NativeHttpV2Server::new_managed(
                fixture.service.handle(),
                NativeHttpV2Config::default(),
            )?;
            let app = server.test_router();
            let started = Arc::new(Barrier::new(2));
            let worker_started = Arc::clone(&started);
            let (release, released) = std::sync::mpsc::sync_channel(0);
            let blocker = tokio::task::spawn_blocking(move || {
                worker_started.wait();
                released.recv()
            });
            started.wait();

            let mut tasks = Vec::new();
            for request_id in 1..=super::server::MAX_HTTP_REQUESTS + 32 {
                let request = Request::builder()
                    .method("GET")
                    .uri("/v2/capabilities")
                    .header(hyphae_contracts::v2::PROTOCOL_MINOR_HEADER_V2, "3")
                    .header(REQUEST_ID_HEADER, request_id.to_string())
                    .header(
                        header::AUTHORIZATION,
                        format!("Bearer {}", fixture.credential),
                    )
                    .body(Body::empty())?;
                let app = app.clone();
                tasks.push(tokio::spawn(async move { app.oneshot(request).await }));
            }
            tokio::time::timeout(Duration::from_secs(10), async {
                while server.test_request_permits() != 0 {
                    tokio::task::yield_now().await;
                }
                while tasks.iter().filter(|task| task.is_finished()).count() < 32 {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            assert_eq!(server.test_request_permits(), 0);
            assert!(server.test_authentication_permits() <= super::server::MAX_HTTP_REQUESTS);
            release.send(())?;
            blocker.await??;
            let mut unavailable = 0;
            for task in tasks {
                if task.await??.status() == StatusCode::SERVICE_UNAVAILABLE {
                    unavailable += 1;
                }
            }
            assert_eq!(unavailable, 32);
            Ok::<(), Box<dyn Error>>(())
        })
    }

    #[tokio::test]
    async fn malformed_or_unauthorized_headers_never_consume_body_admission()
    -> Result<(), Box<dyn Error>> {
        let fixture = managed_service("header-before-body-admission", false)?;
        let server = NativeHttpV2Server::new_managed(
            fixture.service.handle(),
            NativeHttpV2Config::default(),
        )?;
        let app = server.test_router();
        let baseline = server.state.body_slots.available_permits();

        let oversized = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v2/execute")
                    .header(header::CONTENT_TYPE, PRODUCT_MEDIA_TYPE)
                    .header(hyphae_contracts::v2::PROTOCOL_MINOR_HEADER_V2, "3")
                    .header(header::CONTENT_LENGTH, (16 * 1024 * 1024 + 1).to_string())
                    .header(
                        header::AUTHORIZATION,
                        format!("Bearer {}", fixture.credential),
                    )
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(server.state.body_slots.available_permits(), baseline);

        let unauthorized = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v2/execute")
                    .header(header::CONTENT_TYPE, PRODUCT_MEDIA_TYPE)
                    .header(hyphae_contracts::v2::PROTOCOL_MINOR_HEADER_V2, "3")
                    .header(header::AUTHORIZATION, "Bearer not-a-key")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(server.state.body_slots.available_permits(), baseline);

        let retained = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let (_release, released) = watch::channel(false);
        let mut tasks = Vec::new();
        for request_id in 1..=super::server::MAX_HTTP_BODY_READERS + 32 {
            let request = Request::builder()
                .method("POST")
                .uri("/v2/execute")
                .header(header::CONTENT_TYPE, PRODUCT_MEDIA_TYPE)
                .header(hyphae_contracts::v2::PROTOCOL_MINOR_HEADER_V2, "3")
                .header(REQUEST_ID_HEADER, request_id.to_string())
                .header(header::AUTHORIZATION, "Bearer not-a-key")
                .body(Body::from_stream(RetainedSlowBody::new(
                    Arc::clone(&retained),
                    Arc::clone(&maximum),
                    released.clone(),
                )))?;
            let app = app.clone();
            tasks.push(tokio::spawn(async move { app.oneshot(request).await }));
        }
        for task in tasks {
            assert_eq!(task.await??.status(), StatusCode::UNAUTHORIZED);
        }
        assert_eq!(maximum.load(Ordering::SeqCst), 0);
        assert_eq!(retained.load(Ordering::SeqCst), 0);
        assert_eq!(server.state.body_slots.available_permits(), baseline);
        drop(fixture.service);
        Ok(())
    }

    #[test]
    fn remote_bind_policy_is_validated_before_session_or_socket() -> Result<(), Box<dyn Error>> {
        let (_directory, service) = service("remote")?;
        let config = NativeHttpV2Config {
            bind: (Ipv4Addr::UNSPECIFIED, 0).into(),
            ..NativeHttpV2Config::default()
        };
        assert!(matches!(
            NativeHttpV2Server::new(service.handle(), config),
            Err(super::NativeHttpV2Error::Configuration(
                NativeHttpV2ConfigError::RemoteBindRequiresAuthentication { .. }
            ))
        ));
        let managed = NativeHttpV2Server::new_managed(
            service.handle(),
            NativeHttpV2Config {
                bind: (Ipv4Addr::UNSPECIFIED, 0).into(),
                ..NativeHttpV2Config::default()
            },
        );
        assert!(matches!(
            managed,
            Err(super::NativeHttpV2Error::Configuration(
                NativeHttpV2ConfigError::RemoteManagedBindRequiresTls { .. }
            ))
        ));

        let conflicting = NativeHttpV2Server::new_managed(
            service.handle(),
            NativeHttpV2Config {
                bearer_token: Some(BearerToken::new("0123456789abcdef0123456789abcdef")?),
                ..NativeHttpV2Config::default()
            },
        );
        assert!(matches!(
            conflicting,
            Err(super::NativeHttpV2Error::Configuration(
                NativeHttpV2ConfigError::ManagedAuthenticationConflict
            ))
        ));
        drop(service);
        Ok(())
    }

    #[tokio::test]
    async fn default_server_requires_managed_credentials_for_bootstrapped_catalogs()
    -> Result<(), Box<dyn Error>> {
        let fixture = managed_service("automatic-managed-auth", false)?;
        let app = NativeHttpV2Server::new(fixture.service.handle(), NativeHttpV2Config::default())?
            .test_router();
        let body = request(ProductOperation::Capabilities)?;
        let wrong_credential = concat!(
            "hyp1_11111111111111111111111111111111_",
            "2222222222222222222222222222222222222222222222222222222222222222"
        );

        for (request_id, candidate) in [("214", None), ("215", Some(wrong_credential))] {
            let response = app
                .clone()
                .oneshot(http_request(
                    "/v2/execute",
                    body.clone(),
                    Some(request_id),
                    candidate,
                    None,
                )?)
                .await?;

            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(
                response.headers()[header::WWW_AUTHENTICATE],
                "Bearer realm=\"hyphae-native-v2\""
            );
            let denied: Value = serde_json::from_slice(&response_bytes(response).await?)?;
            assert_eq!(denied["code"], "authorization_denied");
            assert_eq!(denied["request_id"], request_id);
        }
        Ok(())
    }

    #[test]
    fn default_server_rejects_legacy_bearer_for_bootstrapped_catalogs() -> Result<(), Box<dyn Error>>
    {
        let fixture = managed_service("automatic-managed-conflict", false)?;
        let configured = NativeHttpV2Server::new(
            fixture.service.handle(),
            NativeHttpV2Config {
                bearer_token: Some(BearerToken::new("0123456789abcdef0123456789abcdef")?),
                ..NativeHttpV2Config::default()
            },
        );

        assert!(matches!(
            configured,
            Err(super::NativeHttpV2Error::Configuration(
                NativeHttpV2ConfigError::ManagedAuthenticationConflict
            ))
        ));
        Ok(())
    }

    #[tokio::test]
    async fn default_server_preserves_unmanaged_access_for_empty_catalogs()
    -> Result<(), Box<dyn Error>> {
        let (_directory, service) = service("automatic-unmanaged-empty")?;
        let app =
            NativeHttpV2Server::new(service.handle(), NativeHttpV2Config::default())?.test_router();
        let response = app
            .oneshot(http_request(
                "/v2/execute",
                request(ProductOperation::Capabilities)?,
                Some("216"),
                None,
                None,
            )?)
            .await?;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::WWW_AUTHENTICATE));
        assert!(matches!(
            decode_product_response(&response_bytes(response).await?)?,
            hyphae_native_product::ProductResponse::Capabilities(_)
        ));
        Ok(())
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn migrated_legacy_bearer_is_http_only_cannot_fallback_from_hyp1_and_revokes_live_session()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("legacy-migration-window");
        fs::create_dir_all(&directory.0)?;
        let data = directory.0.join("data");
        let key_path = directory.0.join("canonical.key");
        let legacy = "legacy-bearer-canary-0123456789abcdef";
        let mut product = NativeProduct::create(&data)?;
        drop(product);
        product = NativeProduct::open_offline_owner(&data)?;
        let started = product.start_legacy_bearer_migration_offline(
            "Migrated owner",
            "canonical",
            legacy.as_bytes(),
            1,
        )?;
        let canonical = started.secret.expose_secret().to_owned();
        let pending_service =
            NativeProductService::start(product, NativeProductServiceConfig::default())?;
        NativeHttpV2Server::new(
            pending_service.handle(),
            NativeHttpV2Config {
                legacy_bearer_token: Some(BearerToken::new(legacy)?),
                ..NativeHttpV2Config::default()
            },
        )?;
        product = pending_service.shutdown()?;
        fs::write(&key_path, &canonical)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))?;
        }
        product.activate_legacy_bearer_migration_offline(
            started.key_id,
            &canonical,
            started.authorization_epoch,
            "Migrated owner",
            "canonical",
            legacy.as_bytes(),
            2,
        )?;
        let service = NativeProductService::start(product, NativeProductServiceConfig::default())?;
        let app = NativeHttpV2Server::new(
            service.handle(),
            NativeHttpV2Config {
                legacy_bearer_token: Some(BearerToken::new(legacy)?),
                ..NativeHttpV2Config::default()
            },
        )?
        .test_router();

        let accepted = app
            .clone()
            .oneshot(http_request(
                "/v2/execute",
                request(ProductOperation::TransactionBegin)?,
                Some("801"),
                Some(legacy),
                None,
            )?)
            .await?;
        assert_eq!(accepted.status(), StatusCode::OK);
        let retained = accepted.headers()[hyphae_contracts::v2::SESSION_ID_HEADER_V2]
            .to_str()?
            .to_owned();
        let ProductResponse::ExplicitTransactionStatus(
            hyphae_native_product::ProductExplicitTransactionStatus::Active { handle, .. },
        ) = decode_product_response(&response_bytes(accepted).await?)?
        else {
            return Err("legacy session did not begin a transaction".into());
        };

        let canonical_looking_legacy = concat!(
            "hyp1_11111111111111111111111111111111_",
            "2222222222222222222222222222222222222222222222222222222222222222"
        );
        let denied = app
            .clone()
            .oneshot(http_request(
                "/v2/execute",
                request(ProductOperation::Capabilities)?,
                Some("802"),
                Some(canonical_looking_legacy),
                None,
            )?)
            .await?;
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

        let owner = service.handle().open_authenticated_session(
            hyphae_native_product::ApiKeyCredential::new(&canonical)?,
        )?;
        let mut context = owner.request_context(803, 3);
        context.idempotency_token = Some(803);
        owner.dispatch(context, ProductOperation::SecurityLegacyBearerRevoke)?;

        let retained_denied = app
            .clone()
            .oneshot(http_request_with_session(
                "/v2/execute",
                request(ProductOperation::TransactionRollback { handle })?,
                "804",
                Some(legacy),
                Some(&retained),
            )?)
            .await?;
        assert_eq!(retained_denied.status(), StatusCode::FORBIDDEN);
        let fresh_denied = app
            .oneshot(http_request(
                "/v2/execute",
                request(ProductOperation::Capabilities)?,
                Some("805"),
                Some(legacy),
                None,
            )?)
            .await?;
        assert_eq!(fresh_denied.status(), StatusCode::UNAUTHORIZED);
        drop(owner);
        drop(service);
        Ok(())
    }

    #[test]
    fn migrated_legacy_bearer_configuration_must_match_exactly() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("legacy-exact-config");
        fs::create_dir_all(&directory.0)?;
        let data = directory.0.join("data");
        let legacy_a = "legacy-bearer-a-0123456789abcdef012345";
        let legacy_b = "legacy-bearer-b-0123456789abcdef012345";
        drop(NativeProduct::create(&data)?);
        let mut product = NativeProduct::open_offline_owner(&data)?;
        let started = product.start_legacy_bearer_migration_offline(
            "Migrated owner",
            "canonical",
            legacy_a.as_bytes(),
            1,
        )?;
        let canonical = started.secret.expose_secret().to_owned();
        product.activate_legacy_bearer_migration_offline(
            started.key_id,
            &canonical,
            started.authorization_epoch,
            "Migrated owner",
            "canonical",
            legacy_a.as_bytes(),
            2,
        )?;
        let service = NativeProductService::start(product, NativeProductServiceConfig::default())?;
        NativeHttpV2Server::new(
            service.handle(),
            NativeHttpV2Config {
                legacy_bearer_token: Some(BearerToken::new(legacy_a)?),
                ..NativeHttpV2Config::default()
            },
        )?;
        let wrong = NativeHttpV2Server::new(
            service.handle(),
            NativeHttpV2Config {
                legacy_bearer_token: Some(BearerToken::new(legacy_b)?),
                ..NativeHttpV2Config::default()
            },
        );
        assert!(matches!(
            wrong,
            Err(super::NativeHttpV2Error::Configuration(
                NativeHttpV2ConfigError::LegacyBearerMismatch
            ))
        ));
        Ok(())
    }

    #[test]
    fn compatibility_version_1_3_rejects_enabled_legacy_state_but_preserves_revoked_startup()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("legacy-version-gate");
        fs::create_dir_all(&directory.0)?;
        let data = directory.0.join("data");
        let legacy = "legacy-version-canary-0123456789abcdef";
        let mut product = NativeProduct::create(&data)?;
        drop(product);
        product = NativeProduct::open_offline_owner(&data)?;
        let started = product.start_legacy_bearer_migration_offline(
            "Migrated owner",
            "canonical",
            legacy.as_bytes(),
            1,
        )?;
        let canonical = started.secret.expose_secret().to_owned();
        product.activate_legacy_bearer_migration_offline(
            started.key_id,
            &canonical,
            started.authorization_epoch,
            "Migrated owner",
            "canonical",
            legacy.as_bytes(),
            2,
        )?;
        let service = NativeProductService::start(product, NativeProductServiceConfig::default())?;
        let version_1_3 =
            hyphae_native_product::LegacyBearerCompatibilityVersion { major: 1, minor: 3 };
        let configured = NativeHttpV2Server::new(
            service.handle(),
            NativeHttpV2Config {
                legacy_bearer_token: Some(BearerToken::new(legacy)?),
                legacy_compatibility_version: version_1_3,
                ..NativeHttpV2Config::default()
            },
        );
        assert!(matches!(
            configured,
            Err(super::NativeHttpV2Error::Configuration(
                NativeHttpV2ConfigError::LegacyCompatibilityExpired { .. }
            ))
        ));
        let owner = service.handle().open_authenticated_session(
            hyphae_native_product::ApiKeyCredential::new(&canonical)?,
        )?;
        let mut context = owner.request_context(901, 3);
        context.idempotency_token = Some(901);
        owner.dispatch(context, ProductOperation::SecurityLegacyBearerRevoke)?;
        NativeHttpV2Server::new(
            service.handle(),
            NativeHttpV2Config {
                legacy_compatibility_version: version_1_3,
                ..NativeHttpV2Config::default()
            },
        )?;
        Ok(())
    }

    #[test]
    fn normal_bootstrap_never_enables_legacy_and_state_requires_exact_configuration()
    -> Result<(), Box<dyn Error>> {
        let fixture = managed_service("legacy-state-config-matrix", false)?;
        assert_eq!(
            fixture.service.handle().legacy_bearer_state(),
            hyphae_native_product::LegacyBearerState::NeverEnabled
        );
        let configured = NativeHttpV2Server::new(
            fixture.service.handle(),
            NativeHttpV2Config {
                legacy_bearer_token: Some(BearerToken::new(
                    "normal-bootstrap-cannot-enable-legacy",
                )?),
                ..NativeHttpV2Config::default()
            },
        );
        assert!(matches!(
            configured,
            Err(super::NativeHttpV2Error::Configuration(
                NativeHttpV2ConfigError::LegacyStateConfigurationMismatch { .. }
            ))
        ));
        NativeHttpV2Server::new(fixture.service.handle(), NativeHttpV2Config::default())?;
        Ok(())
    }

    #[tokio::test]
    async fn authentication_and_request_ids_are_enforced() -> Result<(), Box<dyn Error>> {
        let (_directory, service) = service("auth")?;
        let token = "0123456789abcdef0123456789abcdef";
        let config = NativeHttpV2Config {
            bearer_token: Some(BearerToken::new(token)?),
            ..NativeHttpV2Config::default()
        };
        let app = NativeHttpV2Server::new(service.handle(), config)?.test_router();
        let denied = app
            .clone()
            .oneshot(http_request(
                "/v2/execute",
                request(ProductOperation::Capabilities).map_err(|error| error.to_string())?,
                Some("41"),
                None,
                None,
            )?)
            .await?;
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(denied.headers()[REQUEST_ID_HEADER], "41");
        let denied_body: Value = serde_json::from_slice(&response_bytes(denied).await?)?;
        assert_eq!(denied_body["code"], "authorization_denied");
        assert_eq!(denied_body["request_id"], "41");

        let accepted = app
            .clone()
            .oneshot(http_request(
                "/v2/execute",
                request(ProductOperation::Capabilities).map_err(|error| error.to_string())?,
                Some("42"),
                Some(token),
                None,
            )?)
            .await?;
        assert_eq!(accepted.status(), StatusCode::OK);
        assert_eq!(accepted.headers()[REQUEST_ID_HEADER], "42");
        assert!(matches!(
            decode_product_response(&response_bytes(accepted).await?)?,
            hyphae_native_product::ProductResponse::Capabilities(_)
        ));
        drop(service);
        Ok(())
    }

    #[tokio::test]
    async fn managed_authentication_requires_exact_api_key_bearers() -> Result<(), Box<dyn Error>> {
        let fixture = managed_service("managed-auth", false)?;
        let app = NativeHttpV2Server::new_managed(
            fixture.service.handle(),
            NativeHttpV2Config::default(),
        )?
        .test_router();
        let body = request(ProductOperation::Capabilities)?;
        let wrong_credential = concat!(
            "hyp1_11111111111111111111111111111111_",
            "2222222222222222222222222222222222222222222222222222222222222222"
        );

        for (request_id, candidate) in [
            ("201", None),
            ("202", Some("hyp1_short")),
            ("203", Some(wrong_credential)),
        ] {
            let response = app
                .clone()
                .oneshot(http_request(
                    "/v2/execute",
                    body.clone(),
                    Some(request_id),
                    candidate,
                    None,
                )?)
                .await?;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(
                response.headers()[header::WWW_AUTHENTICATE],
                "Bearer realm=\"hyphae-native-v2\""
            );
            let denied: Value = serde_json::from_slice(&response_bytes(response).await?)?;
            assert_eq!(denied["code"], "authorization_denied");
            assert_eq!(denied["request_id"], request_id);
        }

        let mut wrong_scheme = http_request("/v2/execute", body.clone(), Some("209"), None, None)?;
        wrong_scheme.headers_mut().insert(
            header::AUTHORIZATION,
            format!("Basic {}", fixture.credential).parse()?,
        );
        let wrong_scheme = app.clone().oneshot(wrong_scheme).await?;
        assert_eq!(wrong_scheme.status(), StatusCode::UNAUTHORIZED);
        assert!(
            wrong_scheme
                .headers()
                .contains_key(header::WWW_AUTHENTICATE)
        );

        let mut duplicated = http_request(
            "/v2/execute",
            body.clone(),
            Some("213"),
            Some(&fixture.credential),
            None,
        )?;
        duplicated.headers_mut().append(
            header::AUTHORIZATION,
            format!("Bearer {}", fixture.credential).parse()?,
        );
        let duplicated = app.clone().oneshot(duplicated).await?;
        assert_eq!(duplicated.status(), StatusCode::UNAUTHORIZED);
        assert!(duplicated.headers().contains_key(header::WWW_AUTHENTICATE));

        let accepted = app
            .clone()
            .oneshot(http_request(
                "/v2/execute",
                body,
                Some("204"),
                Some(&fixture.credential),
                None,
            )?)
            .await?;
        assert_eq!(accepted.status(), StatusCode::OK);
        Ok(())
    }

    #[tokio::test]
    async fn managed_http_sessions_are_bound_to_the_opening_credential()
    -> Result<(), Box<dyn Error>> {
        let fixture = managed_service("managed-session-binding", false)?;
        let app = NativeHttpV2Server::new_managed(
            fixture.service.handle(),
            NativeHttpV2Config::default(),
        )?
        .test_router();
        let begun = app
            .clone()
            .oneshot(http_request_with_session(
                "/v2/execute",
                request(ProductOperation::TransactionBegin)?,
                "206",
                Some(&fixture.credential),
                None,
            )?)
            .await?;
        assert_eq!(begun.status(), StatusCode::OK);
        let session_id = begun.headers()[hyphae_contracts::v2::SESSION_ID_HEADER_V2]
            .to_str()?
            .to_owned();
        let hyphae_native_product::ProductResponse::ExplicitTransactionStatus(
            hyphae_native_product::ProductExplicitTransactionStatus::Active { handle, .. },
        ) = decode_product_response(&response_bytes(begun).await?)?
        else {
            return Err("managed transaction begin returned the wrong response".into());
        };

        let wrong_credential = concat!(
            "hyp1_11111111111111111111111111111111_",
            "2222222222222222222222222222222222222222222222222222222222222222"
        );
        let wrong_for_session = app
            .clone()
            .oneshot(http_request_with_session(
                "/v2/execute",
                request(ProductOperation::TransactionRollback { handle })?,
                "207",
                Some(wrong_credential),
                Some(&session_id),
            )?)
            .await?;
        assert_eq!(wrong_for_session.status(), StatusCode::UNAUTHORIZED);
        assert!(
            wrong_for_session
                .headers()
                .contains_key(header::WWW_AUTHENTICATE)
        );

        let rolled_back = app
            .oneshot(http_request_with_session(
                "/v2/execute",
                request(ProductOperation::TransactionRollback { handle })?,
                "208",
                Some(&fixture.credential),
                Some(&session_id),
            )?)
            .await?;
        assert_eq!(rolled_back.status(), StatusCode::OK);
        Ok(())
    }

    #[tokio::test]
    async fn fresh_http_request_replays_self_revoke_after_ack_loss() -> Result<(), Box<dyn Error>> {
        let fixture = managed_service("self-revoke-reconnect", true)?;
        let app = NativeHttpV2Server::new_managed(
            fixture.service.handle(),
            NativeHttpV2Config::default(),
        )?
        .test_router();
        let operation = ProductOperation::SecurityApiKeyRevokeSelf {
            key_id: fixture.credential_key_id,
        };
        let first = app
            .clone()
            .oneshot(http_request(
                "/v2/security/keys",
                mutation_request(operation.clone(), 0x7b01)?,
                Some("301"),
                Some(&fixture.credential),
                None,
            )?)
            .await?;
        assert_eq!(first.status(), StatusCode::OK);
        drop(first);

        let replay = app
            .oneshot(http_request(
                "/v2/security/keys",
                mutation_request(operation, 0x7b01)?,
                Some("302"),
                Some(&fixture.credential),
                None,
            )?)
            .await?;
        assert_eq!(replay.status(), StatusCode::OK);
        assert!(matches!(
            decode_product_response(&response_bytes(replay).await?)?,
            ProductResponse::SecurityMutated(_)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn pending_terminal_is_uniformly_rejected_without_exact_lifecycle_replay()
    -> Result<(), Box<dyn Error>> {
        let fixture = managed_service("revoked-no-oracle", false)?;
        let handle = fixture.service.handle();
        let actor = handle.open_authenticated_session(
            hyphae_native_product::ApiKeyCredential::new(&fixture.credential)?,
        )?;
        actor.dispatch(
            actor.request_context(1, 1).with_idempotency_token(1),
            ProductOperation::SecurityApiKeyRevokeSelf {
                key_id: fixture.credential_key_id,
            },
        )?;
        let app =
            NativeHttpV2Server::new_managed(handle, NativeHttpV2Config::default())?.test_router();
        let unknown = concat!(
            "hyp1_11111111111111111111111111111111_",
            "2222222222222222222222222222222222222222222222222222222222222222"
        );
        let credentials = [
            ("revoked", fixture.credential.as_str()),
            ("unknown", unknown),
            ("malformed", "hyp1_short"),
        ];
        let mut expected = None;
        for path in [
            "/v2/capabilities",
            "/v2/execute",
            "/v2/sql",
            "/v2/security/keys",
        ] {
            for (case, credential) in credentials {
                let request = if path == "/v2/capabilities" {
                    Request::builder()
                        .method("GET")
                        .uri(path)
                        .header(hyphae_contracts::v2::PROTOCOL_MINOR_HEADER_V2, "3")
                        .header(REQUEST_ID_HEADER, "401")
                        .header(header::AUTHORIZATION, format!("Bearer {credential}"))
                        .body(Body::empty())?
                } else {
                    http_request(
                        path,
                        request(ProductOperation::Capabilities)?,
                        Some("401"),
                        Some(credential),
                        None,
                    )?
                };
                let actual = response_snapshot(app.clone().oneshot(request).await?).await?;
                assert_eq!(actual.0, StatusCode::UNAUTHORIZED, "{case} on {path}");
                assert_eq!(
                    actual.1[header::WWW_AUTHENTICATE],
                    "Bearer realm=\"hyphae-native-v2\"",
                    "{case} on {path}"
                );
                if let Some(expected) = &expected {
                    assert_eq!(&actual, expected, "{case} differs on {path}");
                } else {
                    expected = Some(actual);
                }
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn fresh_http_request_replays_zero_overlap_self_rotation_after_ack_loss()
    -> Result<(), Box<dyn Error>> {
        let fixture = managed_service("self-rotate-reconnect", false)?;
        let app = NativeHttpV2Server::new_managed(
            fixture.service.handle(),
            NativeHttpV2Config::default(),
        )?
        .test_router();
        let started = app
            .clone()
            .oneshot(http_request(
                "/v2/security/keys",
                mutation_request(
                    ProductOperation::SecurityApiKeyRotateSelfStart {
                        predecessor_key_id: fixture.credential_key_id,
                        label: "zero-overlap".to_owned(),
                        overlap_seconds: 0,
                        expires_at_micros: None,
                    },
                    0x7b02,
                )?,
                Some("303"),
                Some(&fixture.credential),
                None,
            )?)
            .await?;
        assert_eq!(started.status(), StatusCode::OK);
        let ProductResponse::SecurityApiKeyStarted(started) =
            decode_product_response(&response_bytes(started).await?)?
        else {
            return Err("rotation start returned the wrong response".into());
        };
        let successor = started.secret.take().ok_or("missing successor secret")?;
        let operation = ProductOperation::SecurityApiKeyRotateSelfActivate {
            successor_key_id: started.key_id,
            confirmation_digest: successor.confirmation_digest(),
        };
        let first = app
            .clone()
            .oneshot(http_request(
                "/v2/security/keys",
                mutation_request(operation.clone(), 0x7b03)?,
                Some("304"),
                Some(&fixture.credential),
                None,
            )?)
            .await?;
        assert_eq!(first.status(), StatusCode::OK);
        drop(first);

        let replay = app
            .oneshot(http_request(
                "/v2/security/keys",
                mutation_request(operation, 0x7b03)?,
                Some("305"),
                Some(&fixture.credential),
                None,
            )?)
            .await?;
        assert_eq!(replay.status(), StatusCode::OK);
        assert!(matches!(
            decode_product_response(&response_bytes(replay).await?)?,
            ProductResponse::SecurityApiKeyActivated(ref receipt)
                if receipt.key_id == started.key_id
                    && receipt.predecessor_key_id == Some(fixture.credential_key_id)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn managed_permission_denial_is_typed_forbidden_without_challenge()
    -> Result<(), Box<dyn Error>> {
        let fixture = managed_service("managed-permission", true)?;
        let app = NativeHttpV2Server::new_managed(
            fixture.service.handle(),
            NativeHttpV2Config::default(),
        )?
        .test_router();
        let response = app
            .oneshot(http_request(
                "/v2/structures",
                request(ProductOperation::StructureSet {
                    key: b"denied".to_vec(),
                    value: b"value".to_vec(),
                    expires_at_micros: None,
                })?,
                Some("205"),
                Some(&fixture.credential),
                None,
            )?)
            .await?;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(!response.headers().contains_key(header::WWW_AUTHENTICATE));
        let denied: Value = serde_json::from_slice(&response_bytes(response).await?)?;
        assert_eq!(denied["code"], "authorization_denied");
        assert_eq!(denied["request_id"], "205");
        Ok(())
    }

    #[tokio::test]
    async fn retained_managed_session_cannot_turn_pending_terminal_into_forbidden()
    -> Result<(), Box<dyn Error>> {
        let fixture = managed_service("managed-session-revocation", true)?;
        let handle = fixture.service.handle();
        let app = NativeHttpV2Server::new_managed(handle.clone(), NativeHttpV2Config::default())?
            .test_router();

        let created = app
            .clone()
            .oneshot(http_request(
                "/v2/sql",
                request(ProductOperation::ExecuteSql {
                    statement: "CREATE TABLE revocation_items (id BIGINT PRIMARY KEY)".to_owned(),
                    parameters: Vec::new(),
                })?,
                Some("210"),
                Some(&fixture.owner_credential),
                None,
            )?)
            .await?;
        assert_eq!(created.status(), StatusCode::OK);
        let _ = response_bytes(created).await?;

        let prepared = app
            .clone()
            .oneshot(http_request_with_session(
                "/v2/sql",
                request(ProductOperation::PrepareSql {
                    statement: "SELECT id FROM revocation_items WHERE id = ?".to_owned(),
                })?,
                "211",
                Some(&fixture.credential),
                None,
            )?)
            .await?;
        assert_eq!(prepared.status(), StatusCode::OK);
        let session_id = prepared.headers()[hyphae_contracts::v2::SESSION_ID_HEADER_V2]
            .to_str()?
            .to_owned();
        let hyphae_native_product::ProductResponse::PreparedSql {
            handle: prepared_handle,
            ..
        } = decode_product_response(&response_bytes(prepared).await?)?
        else {
            return Err("managed prepare returned the wrong response".into());
        };

        let owner = handle.open_authenticated_session(
            hyphae_native_product::ApiKeyCredential::new(&fixture.owner_credential)?,
        )?;
        owner.dispatch(
            owner.request_context(999, 6).with_idempotency_token(999),
            ProductOperation::SecurityApiKeyRevoke {
                key_id: fixture.credential_key_id,
            },
        )?;
        let revoked = app
            .oneshot(http_request_with_session(
                "/v2/sql",
                request(ProductOperation::ExecutePrepared {
                    handle: prepared_handle,
                    parameters: vec![hyphae_native_product::ProductValue::Signed(1)],
                })?,
                "212",
                Some(&fixture.credential),
                Some(&session_id),
            )?)
            .await?;

        assert_eq!(revoked.status(), StatusCode::UNAUTHORIZED);
        assert!(revoked.headers().contains_key(header::WWW_AUTHENTICATE));
        let denied: Value = serde_json::from_slice(&response_bytes(revoked).await?)?;
        assert_eq!(denied["code"], "authorization_denied");
        assert_eq!(denied["request_id"], "212");
        Ok(())
    }

    #[tokio::test]
    async fn request_and_response_bounds_fail_with_product_errors() -> Result<(), Box<dyn Error>> {
        let (_directory, service) = service("bounds")?;
        let config = NativeHttpV2Config {
            limits: NativeHttpV2Limits {
                request_body_bytes: 64,
                response_bytes: 128,
                stream_chunk_bytes: 64,
                ..NativeHttpV2Limits::default()
            },
            ..NativeHttpV2Config::default()
        };
        let app = NativeHttpV2Server::new(service.handle(), config)?.test_router();
        let oversized = app
            .oneshot(http_request(
                "/v2/execute",
                vec![0; 65],
                Some("51"),
                None,
                None,
            )?)
            .await?;
        assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let value: Value = serde_json::from_slice(&response_bytes(oversized).await?)?;
        assert_eq!(value["code"], "limit_exceeded");
        assert_eq!(value["category"], "limit");
        assert_eq!(value["retry"], "never");
        assert_eq!(value["transaction_state"], "none");
        assert!(value.get("details").is_some());
        drop(service);
        Ok(())
    }

    #[tokio::test]
    async fn errors_offer_json_parity_and_binary_hyperr01() -> Result<(), Box<dyn Error>> {
        let (_directory, service) = service("errors")?;
        let app =
            NativeHttpV2Server::new(service.handle(), NativeHttpV2Config::default())?.test_router();
        let malformed = app
            .clone()
            .oneshot(http_request(
                "/v2/execute",
                vec![1, 2, 3],
                Some("61"),
                None,
                None,
            )?)
            .await?;
        assert_eq!(
            malformed.headers()[header::CONTENT_TYPE],
            "application/json"
        );
        let value: Value = serde_json::from_slice(&response_bytes(malformed).await?)?;
        for field in [
            "code",
            "category",
            "retry",
            "message",
            "request_id",
            "transaction_state",
            "details",
        ] {
            assert!(value.get(field).is_some(), "{field}");
        }

        let binary = app
            .oneshot(http_request(
                "/v2/execute",
                vec![1, 2, 3],
                Some("62"),
                None,
                Some(ERROR_MEDIA_TYPE),
            )?)
            .await?;
        assert_eq!(binary.headers()[header::CONTENT_TYPE], ERROR_MEDIA_TYPE);
        let error = hyphae_native_protocol::decode_failure(&response_bytes(binary).await?)?;
        assert_eq!(error.request_id(), Some(62));
        assert_eq!(
            error.code(),
            hyphae_native_product::ProductErrorCode::InvalidRequest
        );
        drop(service);
        Ok(())
    }

    #[tokio::test]
    async fn native_server_uses_existing_owner_and_v1_fails_unmappable()
    -> Result<(), Box<dyn Error>> {
        let (directory, service) = service("authority")?;
        let app =
            NativeHttpV2Server::new(service.handle(), NativeHttpV2Config::default())?.test_router();
        let second = NativeProduct::open(&directory.0).expect_err("second authority must fail");
        assert_eq!(
            second.code(),
            hyphae_native_product::ProductErrorCode::DataDirectoryLocked
        );
        let compatibility = app
            .oneshot(http_request(
                "/v1/query",
                Vec::new(),
                Some("71"),
                None,
                None,
            )?)
            .await?;
        assert_eq!(compatibility.status(), StatusCode::CONFLICT);
        let value: Value = serde_json::from_slice(&response_bytes(compatibility).await?)?;
        assert_eq!(value["code"], "invalid_request");
        drop(service);
        Ok(())
    }

    #[tokio::test]
    async fn read_stream_is_provisional_until_terminal_completion() -> Result<(), Box<dyn Error>> {
        let (_directory, service) = service("stream")?;
        let app =
            NativeHttpV2Server::new(service.handle(), NativeHttpV2Config::default())?.test_router();
        let response = app
            .oneshot(http_request(
                "/v2/read-stream",
                request(ProductOperation::Capabilities)?,
                Some("81"),
                None,
                None,
            )?)
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/x-ndjson"
        );
        assert_eq!(response.headers()["x-hyphae-stream-completion"], "required");
        let bytes = response_bytes(response).await?;
        let records = std::str::from_utf8(&bytes)?
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<Result<Vec<_>, _>>()?;
        assert!(
            records[..records.len() - 1]
                .iter()
                .all(|record| record["provisional"] == true)
        );
        let completion = records.last().ok_or("missing stream completion")?;
        assert_eq!(completion["type"], "completion");
        assert_eq!(completion["status"], "complete");
        assert_eq!(completion["request_id"], "81");
        drop(service);
        Ok(())
    }

    #[tokio::test]
    async fn read_stream_rejects_state_changes_before_execution() -> Result<(), Box<dyn Error>> {
        let (_directory, service) = service("stream-read-only")?;
        let app =
            NativeHttpV2Server::new(service.handle(), NativeHttpV2Config::default())?.test_router();

        for (request_id, operation) in [
            (
                "82",
                ProductOperation::StructureSet {
                    key: b"must-not-change".to_vec(),
                    value: b"value".to_vec(),
                    expires_at_micros: None,
                },
            ),
            ("83", ProductOperation::TransactionBegin),
        ] {
            let response = app
                .clone()
                .oneshot(http_request(
                    "/v2/read-stream",
                    request(operation)?,
                    Some(request_id),
                    None,
                    None,
                )?)
                .await?;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }

        let response = app
            .oneshot(http_request(
                "/v2/structures",
                request(ProductOperation::StructureGet {
                    key: b"must-not-change".to_vec(),
                })?,
                Some("84"),
                None,
                None,
            )?)
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(matches!(
            decode_product_response(&response_bytes(response).await?)?,
            hyphae_native_product::ProductResponse::StructureValue(None)
        ));
        drop(service);
        Ok(())
    }

    #[tokio::test]
    async fn explicit_transactions_create_and_reuse_http_sessions() -> Result<(), Box<dyn Error>> {
        let (_directory, service) = service("explicit-transaction-session")?;
        let app =
            NativeHttpV2Server::new(service.handle(), NativeHttpV2Config::default())?.test_router();

        let created = app
            .clone()
            .oneshot(http_request_with_session(
                "/v2/sql",
                request(ProductOperation::ExecuteSql {
                    statement:
                        "CREATE TABLE http_tx_items (id BIGINT PRIMARY KEY, label TEXT NOT NULL)"
                            .to_owned(),
                    parameters: Vec::new(),
                })?,
                "85",
                None,
                None,
            )?)
            .await?;
        assert_eq!(created.status(), StatusCode::OK);
        let _ = response_bytes(created).await?;

        let begun = app
            .clone()
            .oneshot(http_request_with_session(
                "/v2/execute",
                request(ProductOperation::TransactionBegin)?,
                "86",
                None,
                None,
            )?)
            .await?;
        assert_eq!(begun.status(), StatusCode::OK);
        let session_id = begun
            .headers()
            .get(hyphae_contracts::v2::SESSION_ID_HEADER_V2)
            .ok_or("transaction begin omitted the HTTP session")?
            .to_str()?
            .to_owned();
        let response = decode_product_response(&response_bytes(begun).await?)?;
        let hyphae_native_product::ProductResponse::ExplicitTransactionStatus(
            hyphae_native_product::ProductExplicitTransactionStatus::Active { handle, .. },
        ) = response
        else {
            return Err("transaction begin returned the wrong response".into());
        };

        let staged = app
            .clone()
            .oneshot(http_request_with_session(
                "/v2/execute",
                request(ProductOperation::TransactionStageSql {
                    handle,
                    mutation: hyphae_native_product::ProductTransactionSqlMutation {
                        statement: "INSERT INTO http_tx_items (id, label) VALUES (?, ?)".to_owned(),
                        parameters: vec![
                            hyphae_native_product::ProductValue::Signed(7),
                            hyphae_native_product::ProductValue::Text("committed".to_owned()),
                        ],
                    },
                })?,
                "87",
                None,
                Some(&session_id),
            )?)
            .await?;
        assert_eq!(staged.status(), StatusCode::OK);
        assert!(matches!(
            decode_product_response(&response_bytes(staged).await?)?,
            hyphae_native_product::ProductResponse::TransactionStaged(_)
        ));

        let committed = app
            .clone()
            .oneshot(http_request_with_session(
                "/v2/execute",
                request(ProductOperation::TransactionCommit { handle })?,
                "88",
                None,
                Some(&session_id),
            )?)
            .await?;
        assert_eq!(committed.status(), StatusCode::OK);
        assert!(matches!(
            decode_product_response(&response_bytes(committed).await?)?,
            hyphae_native_product::ProductResponse::TransactionCommitted(_)
        ));

        assert_committed_http_row(app, &session_id).await?;
        drop(service);
        Ok(())
    }

    async fn assert_committed_http_row(
        app: axum::Router,
        session_id: &str,
    ) -> Result<(), Box<dyn Error>> {
        let selected = app
            .oneshot(http_request_with_session(
                "/v2/sql",
                request(ProductOperation::ExecuteSql {
                    statement: "SELECT label FROM http_tx_items WHERE id = ?".to_owned(),
                    parameters: vec![hyphae_native_product::ProductValue::Signed(7)],
                })?,
                "89",
                None,
                Some(session_id),
            )?)
            .await?;
        assert_eq!(selected.status(), StatusCode::OK);
        assert!(matches!(
            decode_product_response(&response_bytes(selected).await?)?,
            hyphae_native_product::ProductResponse::Sql {
                result: hyphae_native_product::ProductSqlResult::Rows { rows, .. },
                ..
            } if rows == vec![vec![hyphae_native_product::ProductValue::Text("committed".to_owned())]]
        ));
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_http_requests_use_independent_service_sessions()
    -> Result<(), Box<dyn Error>> {
        let (_directory, service) = service("concurrent-session")?;
        let app =
            NativeHttpV2Server::new(service.handle(), NativeHttpV2Config::default())?.test_router();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let first_barrier = Arc::clone(&barrier);
        let first_app = app.clone();
        let first = tokio::spawn(async move {
            first_barrier.wait().await;
            first_app
                .oneshot(
                    http_request(
                        "/v2/execute",
                        request(ProductOperation::Capabilities)
                            .map_err(|error| error.to_string())?,
                        Some("91"),
                        None,
                        None,
                    )
                    .map_err(|error| error.to_string())?,
                )
                .await
                .map_err(|error| error.to_string())
        });
        let second_barrier = Arc::clone(&barrier);
        let second = tokio::spawn(async move {
            second_barrier.wait().await;
            app.oneshot(
                http_request(
                    "/v2/execute",
                    request(ProductOperation::Capabilities).map_err(|error| error.to_string())?,
                    Some("92"),
                    None,
                    None,
                )
                .map_err(|error| error.to_string())?,
            )
            .await
            .map_err(|error| error.to_string())
        });
        let first = tokio::time::timeout(Duration::from_secs(5), first).await???;
        let second = tokio::time::timeout(Duration::from_secs(5), second).await???;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(second.status(), StatusCode::OK);
        drop(service);
        Ok(())
    }

    #[test]
    fn two_clients_with_the_same_request_id_cancel_independently() -> Result<(), Box<dyn Error>> {
        let (_directory, service) = service("cancellation-namespace")?;
        let server = NativeHttpV2Server::new(service.handle(), NativeHttpV2Config::default())?;
        let first_session =
            hyphae_native_product::ProductSessionId::new(1).ok_or("invalid first session")?;
        let second_session =
            hyphae_native_product::ProductSessionId::new(2).ok_or("invalid second session")?;
        let first = server
            .test_register_cancellation(first_session, 91)
            .map_err(|_| "first cancellation registration failed")?;
        let second = server
            .test_register_cancellation(second_session, 91)
            .map_err(|_| "second cancellation registration failed")?;
        assert!(server.test_cancellation(91).is_none());
        let first_token = first.token.clone();
        let second_token = second.token.clone();
        let barrier = Arc::new(Barrier::new(2));
        let first_barrier = Arc::clone(&barrier);
        let worker = thread::spawn(move || {
            first_barrier.wait();
            drop(first);
        });
        barrier.wait();
        worker.join().map_err(|_| "first client panicked")?;
        assert!(first_token.is_cancelled());
        assert!(!second_token.is_cancelled());
        let remaining = server
            .test_cancellation(91)
            .ok_or("second client cancellation was removed by first client")?;
        assert!(!remaining.is_cancelled());
        drop(second);
        assert!(server.test_cancellation(91).is_none());
        drop(service);
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_session_request_is_rejected_before_mutation_admission()
    -> Result<(), Box<dyn Error>> {
        let (_directory, service) = service("duplicate-request-admission")?;
        let server = NativeHttpV2Server::new(service.handle(), NativeHttpV2Config::default())?;
        let app = server.test_router();
        let created = app
            .clone()
            .oneshot(http_request_with_session(
                "/v2/execute",
                request(ProductOperation::ExecuteSql {
                    statement: "CREATE TABLE duplicate_guard (id BIGINT PRIMARY KEY, value BIGINT)"
                        .to_owned(),
                    parameters: Vec::new(),
                })?,
                "489",
                None,
                None,
            )?)
            .await?;
        assert_eq!(created.status(), StatusCode::OK);
        let _ = response_bytes(created).await?;
        let begun = app
            .clone()
            .oneshot(http_request_with_session(
                "/v2/execute",
                request(ProductOperation::TransactionBegin)?,
                "490",
                None,
                None,
            )?)
            .await?;
        assert_eq!(begun.status(), StatusCode::OK);
        let external_session = begun.headers()[hyphae_contracts::v2::SESSION_ID_HEADER_V2]
            .to_str()?
            .to_owned();
        let external_session_id = u128::from_str_radix(&external_session, 16)?;
        let ProductResponse::ExplicitTransactionStatus(
            hyphae_native_product::ProductExplicitTransactionStatus::Active { handle, .. },
        ) = decode_product_response(&response_bytes(begun).await?)?
        else {
            return Err("transaction begin returned the wrong response".into());
        };
        let product_session = server
            .test_product_session_id(external_session_id)
            .ok_or("HTTP session did not retain a product session")?;
        let original = server
            .test_register_cancellation(product_session, 491)
            .map_err(|_| "failed to reserve original request identity")?;
        let original_token = original.token.clone();

        let duplicate = app
            .clone()
            .oneshot(http_request_with_session(
                "/v2/execute",
                request(ProductOperation::TransactionStageSql {
                    handle,
                    mutation: hyphae_native_product::ProductTransactionSqlMutation {
                        statement: "UPDATE duplicate_guard SET value = 1 WHERE id = 1".to_owned(),
                        parameters: Vec::new(),
                    },
                })?,
                "491",
                None,
                Some(&external_session),
            )?)
            .await?;
        assert_eq!(duplicate.status(), StatusCode::BAD_REQUEST);
        assert!(!original_token.is_cancelled());
        drop(original);

        let status = app
            .oneshot(http_request_with_session(
                "/v2/execute",
                request(ProductOperation::ExplicitTransactionStatus { handle })?,
                "492",
                None,
                Some(&external_session),
            )?)
            .await?;
        let ProductResponse::ExplicitTransactionStatus(
            hyphae_native_product::ProductExplicitTransactionStatus::Active {
                staged_operations,
                ..
            },
        ) = decode_product_response(&response_bytes(status).await?)?
        else {
            return Err("transaction status returned the wrong response".into());
        };
        assert_eq!(staged_operations, 0);
        drop(service);
        Ok(())
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn protocol_minor_is_negotiated_and_emitted_on_every_response()
    -> Result<(), Box<dyn Error>> {
        let (_directory, service) = service("http-minor")?;
        let app =
            NativeHttpV2Server::new(service.handle(), NativeHttpV2Config::default())?.test_router();

        let success = app
            .clone()
            .oneshot(http_request(
                "/v2/execute",
                request(ProductOperation::Capabilities)?,
                Some("493"),
                None,
                None,
            )?)
            .await?;
        assert_eq!(success.status(), StatusCode::OK);
        assert_eq!(
            success.headers()[hyphae_contracts::v2::PROTOCOL_MINOR_HEADER_V2],
            "3"
        );

        let mut unsupported = http_request(
            "/v2/execute",
            request(ProductOperation::Capabilities)?,
            Some("494"),
            None,
            Some(ERROR_MEDIA_TYPE),
        )?;
        unsupported
            .headers_mut()
            .insert(hyphae_contracts::v2::PROTOCOL_MINOR_HEADER_V2, "2".parse()?);
        let unsupported = app.clone().oneshot(unsupported).await?;
        assert_eq!(unsupported.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            unsupported.headers()[hyphae_contracts::v2::PROTOCOL_MINOR_HEADER_V2],
            "6"
        );

        for (request_id, minor) in [("496", None), ("497", Some("garbage"))] {
            let mut request = http_request(
                "/v2/execute",
                request(ProductOperation::Capabilities)?,
                Some(request_id),
                None,
                Some(ERROR_MEDIA_TYPE),
            )?;
            request
                .headers_mut()
                .remove(hyphae_contracts::v2::PROTOCOL_MINOR_HEADER_V2);
            if let Some(minor) = minor {
                request.headers_mut().insert(
                    hyphae_contracts::v2::PROTOCOL_MINOR_HEADER_V2,
                    minor.parse()?,
                );
            }
            let rejected = app.clone().oneshot(request).await?;
            assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                rejected.headers()[hyphae_contracts::v2::PROTOCOL_MINOR_HEADER_V2],
                "6"
            );
        }

        // A bounded multi-minor offer selects the highest served member and
        // echoes the selection; malformed or unserved offers fail closed.
        for (request_id, offer, selected) in [
            ("510", "3,4", "4"),
            ("511", "1, 3", "3"),
            ("512", "4,3,2", "4"),
            ("513", "4", "4"),
            ("508", "3,4,5", "5"),
            ("509", "3,4,5,6", "6"),
            ("521", "6", "6"),
        ] {
            let mut accepted = http_request(
                "/v2/execute",
                request(ProductOperation::Capabilities)?,
                Some(request_id),
                None,
                None,
            )?;
            accepted.headers_mut().insert(
                hyphae_contracts::v2::PROTOCOL_MINOR_HEADER_V2,
                offer.parse()?,
            );
            let accepted = app.clone().oneshot(accepted).await?;
            assert_eq!(accepted.status(), StatusCode::OK);
            assert_eq!(
                accepted.headers()[hyphae_contracts::v2::PROTOCOL_MINOR_HEADER_V2],
                selected
            );
        }
        let mut repeated = http_request(
            "/v2/execute",
            request(ProductOperation::Capabilities)?,
            Some("520"),
            None,
            None,
        )?;
        repeated
            .headers_mut()
            .insert(hyphae_contracts::v2::PROTOCOL_MINOR_HEADER_V2, "5".parse()?);
        repeated
            .headers_mut()
            .append(hyphae_contracts::v2::PROTOCOL_MINOR_HEADER_V2, "3".parse()?);
        let repeated = app.clone().oneshot(repeated).await?;
        assert_eq!(repeated.status(), StatusCode::OK);
        assert_eq!(
            repeated.headers()[hyphae_contracts::v2::PROTOCOL_MINOR_HEADER_V2],
            "5"
        );
        for (request_id, offer) in [
            ("514", "3,3"),
            ("515", "03"),
            ("516", "3,"),
            ("517", "1,2,5,6,7,8,9,10,3"),
            ("518", "7"),
            ("519", "3 4"),
        ] {
            let mut rejected = http_request(
                "/v2/execute",
                request(ProductOperation::Capabilities)?,
                Some(request_id),
                None,
                Some(ERROR_MEDIA_TYPE),
            )?;
            rejected.headers_mut().insert(
                hyphae_contracts::v2::PROTOCOL_MINOR_HEADER_V2,
                offer.parse()?,
            );
            let rejected = app.clone().oneshot(rejected).await?;
            assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                rejected.headers()[hyphae_contracts::v2::PROTOCOL_MINOR_HEADER_V2],
                "6"
            );
        }

        let managed_fixture = managed_service("http-minor-managed", false)?;
        let managed = NativeHttpV2Server::new_managed(
            managed_fixture.service.handle(),
            NativeHttpV2Config::default(),
        )?
        .test_router();
        for (request_id, minor) in [("501", None), ("502", Some("2")), ("503", Some("bad"))] {
            let mut request = http_request(
                "/v2/execute",
                request(ProductOperation::Capabilities)?,
                Some(request_id),
                None,
                Some(ERROR_MEDIA_TYPE),
            )?;
            request
                .headers_mut()
                .remove(hyphae_contracts::v2::PROTOCOL_MINOR_HEADER_V2);
            if let Some(minor) = minor {
                request.headers_mut().insert(
                    hyphae_contracts::v2::PROTOCOL_MINOR_HEADER_V2,
                    minor.parse()?,
                );
            }
            let rejected = managed.clone().oneshot(request).await?;
            assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
            assert!(!rejected.headers().contains_key(header::WWW_AUTHENTICATE));
        }

        let error = app
            .oneshot(http_request(
                "/missing",
                request(ProductOperation::Capabilities)?,
                Some("495"),
                None,
                None,
            )?)
            .await?;
        assert_eq!(error.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            error.headers()[hyphae_contracts::v2::PROTOCOL_MINOR_HEADER_V2],
            "6"
        );
        drop(service);
        Ok(())
    }

    #[tokio::test]
    async fn api_key_lifecycle_requires_dedicated_managed_strict_route()
    -> Result<(), Box<dyn Error>> {
        let fixture = managed_service("key-route-family", false)?;
        let owner = fixture.owner_credential.clone();
        let actor = fixture
            .service
            .handle()
            .open_authenticated_session(hyphae_native_product::ApiKeyCredential::new(&owner)?)?;
        let principal_id: hyphae_native_product::SecurityId =
            actor.request_context(1, 1).principal.identity().parse()?;
        drop(actor);
        let app = NativeHttpV2Server::new_managed(
            fixture.service.handle(),
            NativeHttpV2Config::default(),
        )?
        .test_router();
        let operation = ProductOperation::SecurityApiKeyIssueSelfStart {
            principal_id,
            label: "route-family".to_owned(),
            roles: vec![BuiltInRole::Owner],
            custom_roles: Vec::new(),
            permission_ceiling: ProductAuthorization::ALL,
            scope_ceiling: vec![ProductScope::Instance],
            expires_at_micros: None,
        };

        let generic = app
            .clone()
            .oneshot(http_request(
                "/v2/execute",
                mutation_request(operation.clone(), 0x7c01)?,
                Some("498"),
                Some(&owner),
                Some(ERROR_MEDIA_TYPE),
            )?)
            .await?;
        assert_eq!(generic.status(), StatusCode::BAD_REQUEST);

        let mut non_strict =
            hyphae_native_protocol::encode_product_request(&hyphae_native_protocol::WireRequest {
                operation,
                logical_time_micros: 1,
                deadline_micros: None,
                idempotency_token: Some(0x7c02),
                limits: ProductLimits::default(),
                durability: ProductDurabilityPolicy::STRICT,
            })?;
        non_strict[88] = 2;
        let non_strict = app
            .oneshot(http_request(
                "/v2/security/keys",
                non_strict,
                Some("499"),
                Some(&owner),
                Some(ERROR_MEDIA_TYPE),
            )?)
            .await?;
        assert_eq!(non_strict.status(), StatusCode::BAD_REQUEST);

        let (_unmanaged_directory, unmanaged_service) = service("key-route-unmanaged")?;
        let unmanaged =
            NativeHttpV2Server::new(unmanaged_service.handle(), NativeHttpV2Config::default())?
                .test_router();
        let unmanaged = unmanaged
            .oneshot(http_request(
                "/v2/security/keys",
                mutation_request(
                    ProductOperation::SecurityApiKeyIssueSelfStart {
                        principal_id,
                        label: "unmanaged".to_owned(),
                        roles: vec![BuiltInRole::Owner],
                        custom_roles: Vec::new(),
                        permission_ceiling: ProductAuthorization::ALL,
                        scope_ceiling: vec![ProductScope::Instance],
                        expires_at_micros: None,
                    },
                    0x7c03,
                )?,
                Some("500"),
                None,
                Some(ERROR_MEDIA_TYPE),
            )?)
            .await?;
        assert_eq!(unmanaged.status(), StatusCode::FORBIDDEN);
        Ok(())
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn prepared_handles_are_opaque_and_principal_bound() -> Result<(), Box<dyn Error>> {
        let (_directory, service) = service("prepared-isolation")?;
        let token = "0123456789abcdef0123456789abcdef";
        let app = NativeHttpV2Server::new(
            service.handle(),
            NativeHttpV2Config {
                bearer_token: Some(BearerToken::new(token)?),
                ..NativeHttpV2Config::default()
            },
        )?
        .test_router();

        let created = app
            .clone()
            .oneshot(http_request_with_session(
                "/v2/sql",
                request(ProductOperation::ExecuteSql {
                    statement: "CREATE TABLE prepared_items (id BIGINT PRIMARY KEY)".to_owned(),
                    parameters: Vec::new(),
                })?,
                "100",
                Some(token),
                None,
            )?)
            .await?;
        assert_eq!(created.status(), StatusCode::OK);

        let prepared = app
            .clone()
            .oneshot(http_request_with_session(
                "/v2/sql",
                request(ProductOperation::PrepareSql {
                    statement: "SELECT id FROM prepared_items WHERE id = ?".to_owned(),
                })?,
                "101",
                Some(token),
                None,
            )?)
            .await?;
        assert_eq!(prepared.status(), StatusCode::OK);
        let session_id = prepared.headers()[hyphae_contracts::v2::SESSION_ID_HEADER_V2]
            .to_str()?
            .to_owned();
        assert_eq!(session_id.len(), 32);
        let hyphae_native_product::ProductResponse::PreparedSql { handle, .. } =
            decode_product_response(&response_bytes(prepared).await?)?
        else {
            return Err("prepare returned the wrong response".into());
        };
        assert_ne!(
            handle.get(),
            1,
            "wire handle must not expose service ordinal"
        );

        let second = app
            .clone()
            .oneshot(http_request_with_session(
                "/v2/sql",
                request(ProductOperation::PrepareSql {
                    statement: "SELECT id FROM prepared_items WHERE id = ?".to_owned(),
                })?,
                "102",
                Some(token),
                None,
            )?)
            .await?;
        assert_eq!(second.status(), StatusCode::OK);
        let second_session = second.headers()[hyphae_contracts::v2::SESSION_ID_HEADER_V2]
            .to_str()?
            .to_owned();
        assert_ne!(second_session, session_id);
        let _ = response_bytes(second).await?;

        let foreign = app
            .clone()
            .oneshot(http_request_with_session(
                "/v2/sql",
                request(ProductOperation::ExecutePrepared {
                    handle,
                    parameters: vec![hyphae_native_product::ProductValue::Signed(1)],
                })?,
                "103",
                Some(token),
                Some(&second_session),
            )?)
            .await?;
        assert_eq!(foreign.status(), StatusCode::BAD_REQUEST);

        let other_token = "fedcba9876543210fedcba9876543210";
        let other_principal = app
            .clone()
            .oneshot(http_request_with_session(
                "/v2/sql",
                request(ProductOperation::ExecutePrepared {
                    handle,
                    parameters: vec![hyphae_native_product::ProductValue::Signed(1)],
                })?,
                "104",
                Some(other_token),
                Some(&session_id),
            )?)
            .await?;
        assert_eq!(other_principal.status(), StatusCode::UNAUTHORIZED);

        let cross_handle = app
            .oneshot(http_request_with_session(
                "/v2/sql",
                request(ProductOperation::ExecutePrepared {
                    handle,
                    parameters: vec![hyphae_native_product::ProductValue::Signed(1)],
                })?,
                "105",
                Some(token),
                Some(&session_id),
            )?)
            .await?;
        assert_eq!(cross_handle.status(), StatusCode::OK);
        drop(service);
        Ok(())
    }

    #[tokio::test]
    async fn principal_cannot_cross_use_another_principals_session() -> Result<(), Box<dyn Error>> {
        let (_directory, service) = service("prepared-principal")?;
        let server = NativeHttpV2Server::new(service.handle(), NativeHttpV2Config::default())?;
        let first = super::server::AuthenticatedPrincipal([1; 32]);
        let second = super::server::AuthenticatedPrincipal([2; 32]);
        let metadata = super::server::RequestMetadata {
            request_id: 106,
            binary_errors: false,
        };
        let authentication =
            super::server::NativeHttpV2Server::test_unmanaged_authentication(first);
        let (session_id, session) =
            super::server::create_session(&server.state, authentication, &metadata)
                .map_err(|error| error.error.message().to_owned())?;
        super::server::insert_session(&server.state, session_id, session, &metadata)
            .map_err(|error| error.error.message().to_owned())?;
        let Err(error) =
            super::server::lookup_session(&server.state, session_id, &second, &metadata)
        else {
            return Err("foreign principal accessed a prepared session".into());
        };
        assert_eq!(
            error.error.code(),
            hyphae_native_product::ProductErrorCode::AuthorizationDenied
        );
        drop(service);
        Ok(())
    }

    #[tokio::test]
    async fn expired_deadline_never_returns_success() -> Result<(), Box<dyn Error>> {
        let (_directory, service) = service("deadline-completion")?;
        let app =
            NativeHttpV2Server::new(service.handle(), NativeHttpV2Config::default())?.test_router();
        let mut wire = WireRequest {
            operation: ProductOperation::Capabilities,
            logical_time_micros: 1,
            deadline_micros: Some(1),
            idempotency_token: None,
            limits: ProductLimits::default(),
            durability: ProductDurabilityPolicy::STRICT,
        };
        let mut request = http_request_with_session(
            "/v2/execute",
            encode_product_request(&wire)?,
            "111",
            None,
            None,
        )?;
        request
            .headers_mut()
            .insert(hyphae_contracts::v2::DEADLINE_HEADER_V2, "1".parse()?);
        let response = app.clone().oneshot(request).await?;
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);

        wire.deadline_micros = Some(unix_time_micros().saturating_add(1));
        let mut request = http_request_with_session(
            "/v2/execute",
            encode_product_request(&wire)?,
            "112",
            None,
            None,
        )?;
        request.headers_mut().insert(
            hyphae_contracts::v2::DEADLINE_HEADER_V2,
            wire.deadline_micros
                .ok_or("missing deadline")?
                .to_string()
                .parse()?,
        );
        let response = app.oneshot(request).await?;
        assert_ne!(response.status(), StatusCode::OK);
        drop(service);
        Ok(())
    }

    #[tokio::test]
    async fn dropped_stream_omits_completion_and_releases_bound() -> Result<(), Box<dyn Error>> {
        let (_directory, service) = service("stream-drop")?;
        let config = NativeHttpV2Config {
            limits: NativeHttpV2Limits {
                stream_chunk_bytes: 1,
                ..NativeHttpV2Limits::default()
            },
            ..NativeHttpV2Config::default()
        };
        let app = NativeHttpV2Server::new(service.handle(), config)?.test_router();
        let response = app
            .clone()
            .oneshot(http_request_with_session(
                "/v2/read-stream",
                request(ProductOperation::Capabilities)?,
                "121",
                None,
                None,
            )?)
            .await?;
        let mut stream = response.into_body().into_data_stream();
        let first = futures_util::StreamExt::next(&mut stream)
            .await
            .ok_or("missing provisional stream record")??;
        let first: Value = serde_json::from_slice(&first)?;
        assert_eq!(first["type"], "data");
        drop(stream);

        let response = app
            .oneshot(http_request_with_session(
                "/v2/read-stream",
                request(ProductOperation::Capabilities)?,
                "122",
                None,
                None,
            )?)
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        drop(response);
        drop(service);
        Ok(())
    }

    #[tokio::test]
    async fn dropping_regular_response_cancels_until_body_completion() -> Result<(), Box<dyn Error>>
    {
        let (_directory, service) = service("response-drop")?;
        let server = NativeHttpV2Server::new(service.handle(), NativeHttpV2Config::default())?;
        let app = server.test_router();
        let response = app
            .oneshot(http_request_with_session(
                "/v2/execute",
                request(ProductOperation::Capabilities)?,
                "1221",
                None,
                None,
            )?)
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let cancellation = server
            .test_cancellation(1221)
            .ok_or("request cancellation was not retained with response body")?;
        assert!(!cancellation.is_cancelled());
        drop(response);
        assert!(cancellation.is_cancelled());
        drop(service);
        Ok(())
    }

    #[tokio::test]
    async fn slow_consumer_past_deadline_never_receives_completion() -> Result<(), Box<dyn Error>> {
        let (_directory, service) = service("stream-deadline")?;
        let config = NativeHttpV2Config {
            limits: NativeHttpV2Limits {
                stream_chunk_bytes: 1,
                ..NativeHttpV2Limits::default()
            },
            ..NativeHttpV2Config::default()
        };
        let app = NativeHttpV2Server::new(service.handle(), config)?.test_router();
        let deadline = unix_time_micros().saturating_add(250_000);
        let wire = WireRequest {
            operation: ProductOperation::Capabilities,
            logical_time_micros: 1,
            deadline_micros: Some(deadline),
            idempotency_token: None,
            limits: ProductLimits::default(),
            durability: ProductDurabilityPolicy::STRICT,
        };
        let mut request = http_request_with_session(
            "/v2/read-stream",
            encode_product_request(&wire)?,
            "123",
            None,
            None,
        )?;
        request.headers_mut().insert(
            hyphae_contracts::v2::DEADLINE_HEADER_V2,
            deadline.to_string().parse()?,
        );
        let response = app.oneshot(request).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let mut stream = response.into_body().into_data_stream();
        let first = futures_util::StreamExt::next(&mut stream)
            .await
            .ok_or("missing provisional stream record")??;
        let first: Value = serde_json::from_slice(&first)?;
        assert_eq!(first["type"], "data");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(futures_util::StreamExt::next(&mut stream).await.is_none());
        drop(service);
        Ok(())
    }

    #[test]
    fn integrated_operations_have_dedicated_route_families() -> Result<(), Box<dyn Error>> {
        let search = ProductOperation::SearchCollection {
            collection: hyphae_native_product::ObjectId::new(1)?,
            request: hyphae_native_product::ProductSearchRequest {
                lexical: None,
                vectors: Vec::new(),
                filter: hyphae_native_product::ProductSearchFilter::MatchAll,
                sort: Vec::new(),
                facets: Vec::new(),
                aggregations: Vec::new(),
                limit: 1,
                fusion: None,
                parent_dedupe: None,
                rerank: None,
                highlight: None,
                autocut: None,
                offset: 0,
            },
        };
        assert!(super::server::family_accepts(
            super::server::OperationFamily::SearchCollection,
            &search,
        ));
        assert!(!super::server::family_accepts(
            super::server::OperationFamily::Search,
            &search,
        ));
        Ok(())
    }

    #[test]
    fn generic_family_rejects_every_api_key_lifecycle_variant() -> Result<(), Box<dyn Error>> {
        let principal_id = hyphae_native_product::SecurityId::new(1).ok_or("principal")?;
        let key_id = ApiKeyId::from_bytes([1; 16]).ok_or("key")?;
        let confirmation_digest =
            hyphae_native_product::ApiKeyConfirmationDigest::from_bytes([2; 32]);
        let operations = [
            ProductOperation::SecurityApiKeyIssueSelfStart {
                principal_id,
                label: "self issue".to_owned(),
                roles: vec![BuiltInRole::Reader],
                custom_roles: Vec::new(),
                permission_ceiling: BuiltInRole::Reader.authorization(),
                scope_ceiling: vec![ProductScope::Instance],
                expires_at_micros: None,
            },
            ProductOperation::SecurityApiKeyIssueStart {
                principal_id,
                label: "admin issue".to_owned(),
                roles: vec![BuiltInRole::Reader],
                custom_roles: Vec::new(),
                permission_ceiling: BuiltInRole::Reader.authorization(),
                scope_ceiling: vec![ProductScope::Instance],
                expires_at_micros: None,
            },
            ProductOperation::SecurityApiKeyIssueSelfActivate {
                key_id,
                confirmation_digest,
            },
            ProductOperation::SecurityApiKeyIssueActivate {
                key_id,
                confirmation_digest,
            },
            ProductOperation::SecurityApiKeyRotateSelfStart {
                predecessor_key_id: key_id,
                label: "self rotate".to_owned(),
                overlap_seconds: 0,
                expires_at_micros: None,
            },
            ProductOperation::SecurityApiKeyRotateStart {
                predecessor_key_id: key_id,
                label: "admin rotate".to_owned(),
                overlap_seconds: 0,
                expires_at_micros: None,
            },
            ProductOperation::SecurityApiKeyRotateSelfActivate {
                successor_key_id: key_id,
                confirmation_digest,
            },
            ProductOperation::SecurityApiKeyRotateActivate {
                successor_key_id: key_id,
                confirmation_digest,
            },
            ProductOperation::SecurityApiKeyIssueSelfAbort { key_id },
            ProductOperation::SecurityApiKeyIssueAbort { key_id },
            ProductOperation::SecurityApiKeyRotateSelfAbort {
                successor_key_id: key_id,
            },
            ProductOperation::SecurityApiKeyRotateAbort {
                successor_key_id: key_id,
            },
            ProductOperation::SecurityApiKeyRevokeSelf { key_id },
            ProductOperation::SecurityApiKeyRevoke { key_id },
        ];
        for operation in operations {
            assert!(!super::server::family_accepts(
                super::server::OperationFamily::Any,
                &operation,
            ));
            assert!(super::server::family_accepts(
                super::server::OperationFamily::Security,
                &operation,
            ));
        }
        Ok(())
    }

    #[tokio::test]
    async fn telemetry_and_typed_explain_use_the_same_canonical_http_codec()
    -> Result<(), Box<dyn Error>> {
        let (_directory, service) = service("telemetry-explain")?;
        let app =
            NativeHttpV2Server::new(service.handle(), NativeHttpV2Config::default())?.test_router();
        let telemetry = app
            .clone()
            .oneshot(http_request(
                "/v2/telemetry",
                request(ProductOperation::Telemetry)?,
                Some("141"),
                None,
                None,
            )?)
            .await?;
        let hyphae_native_product::ProductResponse::Telemetry(snapshot) =
            decode_product_response(&response_bytes(telemetry).await?)?
        else {
            return Err("HTTP telemetry returned the wrong response".into());
        };
        assert_ne!(snapshot.process_start_identity, 0);
        assert_ne!(snapshot.session_start_identity, 0);

        let created = app
            .clone()
            .oneshot(http_request(
                "/v2/sql",
                request(ProductOperation::ExecuteSql {
                    statement: "CREATE TABLE explain_items (id BIGINT PRIMARY KEY)".into(),
                    parameters: Vec::new(),
                })?,
                Some("142"),
                None,
                None,
            )?)
            .await?;
        assert_eq!(created.status(), StatusCode::OK);
        let explained = app
            .oneshot(http_request(
                "/v2/admin",
                request(ProductOperation::AdminExplainSql {
                    statement: "SELECT id FROM explain_items WHERE id = 1".into(),
                })?,
                Some("143"),
                None,
                None,
            )?)
            .await?;
        let hyphae_native_product::ProductResponse::Explain(
            hyphae_native_product::ProductExplain::SqlPlanText(plan),
        ) = decode_product_response(&response_bytes(explained).await?)?
        else {
            return Err("HTTP explain returned the wrong response".into());
        };
        assert!(!plan.executed);
        assert!(plan.text.starts_with("PrimaryKeyLookup("));
        drop(service);
        Ok(())
    }

    fn unix_time_micros() -> i64 {
        let micros = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_micros();
        i64::try_from(micros).unwrap_or(i64::MAX)
    }
}
