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
    use std::{error::Error, fs, net::Ipv4Addr, path::PathBuf, sync::Arc, time::Duration};

    use axum::{
        body::{self, Body, Bytes},
        http::{Request, StatusCode, header},
    };
    use hyphae_native_product::{
        NativeProduct, NativeProductService, NativeProductServiceConfig, ProductDurabilityPolicy,
        ProductLimits, ProductOperation,
    };
    use hyphae_native_protocol::{WireRequest, decode_product_response, encode_product_request};
    use serde_json::Value;
    use tower::ServiceExt as _;

    use super::{
        ERROR_MEDIA_TYPE, NativeHttpV2Config, NativeHttpV2ConfigError, NativeHttpV2Limits,
        NativeHttpV2Server, PRODUCT_MEDIA_TYPE, REQUEST_ID_HEADER,
    };
    use crate::BearerToken;

    struct TestDirectory(PathBuf);

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
            .header(header::CONTENT_TYPE, PRODUCT_MEDIA_TYPE);
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

    #[test]
    fn remote_bind_requires_auth_before_session_or_socket() -> Result<(), Box<dyn Error>> {
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
        drop(service);
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
        let (session_id, session) = super::server::create_session(&server.state, first, &metadata)
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
