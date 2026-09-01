// SPDX-License-Identifier: Apache-2.0

use hyphae_native_product::{ApiKeyCredential, MAX_API_KEY_CREDENTIAL_BYTES, ProductCapabilities};
use thiserror::Error;

/// Current native-local protocol major version.
pub const PROTOCOL_MAJOR: u16 = 1;
/// Current native-local protocol minor version.
pub const PROTOCOL_MINOR: u16 = 6;
/// Maximum combined UTF-8 bytes in handshake names.
pub const MAX_HANDSHAKE_TEXT_BYTES: usize = 4 * 1024;
/// Exact UTF-8 bytes in one Native API-key authentication trailer.
pub const API_KEY_AUTH_TRAILER_BYTES: usize = MAX_API_KEY_CREDENTIAL_BYTES;

const HELLO_MAGIC: &[u8; 8] = b"HYPHEL01";
const WELCOME_MAGIC: &[u8; 8] = b"HYPWEL01";
const HELLO_HEADER_SIZE: usize = 58;
const WELCOME_SIZE: usize = 94;
const COMPRESSION_NONE: u8 = 1;
const AUTHENTICATION_API_KEY: u8 = 1;

/// Closed v1 capability bit set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtocolCapabilities(u64);

impl ProtocolCapabilities {
    /// No capabilities.
    pub const NONE: Self = Self(0);
    /// Bounded provisional `DATA` streams completed by `END`.
    pub const STREAM_COMPLETION: Self = Self(1 << 0);
    /// Per-stream byte-window flow control.
    pub const FLOW_CONTROL: Self = Self(1 << 1);
    /// Request-ID cancellation.
    pub const CANCELLATION: Self = Self(1 << 2);
    /// Absolute request deadlines.
    pub const DEADLINES: Self = Self(1 << 3);
    /// Session-local prepared handles.
    pub const PREPARED: Self = Self(1 << 4);
    /// Transport-authenticated peer identity.
    pub const PEER_IDENTITY: Self = Self(1 << 5);
    /// Canonical `HYPERR01` failures.
    pub const PRODUCT_ERRORS: Self = Self(1 << 6);
    /// Managed Native API-key authentication in the `HELLO` trailer.
    pub const API_KEY_AUTH: Self = Self(1 << 7);
    /// Every capability required by the G6 local daemon.
    pub const G6: Self = Self(
        Self::STREAM_COMPLETION.0
            | Self::FLOW_CONTROL.0
            | Self::CANCELLATION.0
            | Self::DEADLINES.0
            | Self::PREPARED.0
            | Self::PEER_IDENTITY.0
            | Self::PRODUCT_ERRORS.0,
    );
    /// G6 local-daemon capabilities with managed API-key authentication.
    pub const G6_AUTHENTICATED: Self = Self(Self::G6.0 | Self::API_KEY_AUTH.0);
    const KNOWN: Self = Self::G6_AUTHENTICATED;

    /// Constructs a bit set while rejecting unknown bits.
    pub const fn from_bits(bits: u64) -> Option<Self> {
        if bits & !Self::KNOWN.0 == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    /// Returns the primitive bit set.
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Returns whether every bit in `other` is present.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
}

/// Client handshake and requested resource envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Hello {
    /// Lowest admitted major version.
    pub minimum_major: u16,
    /// Highest admitted major version.
    pub maximum_major: u16,
    /// Lowest admitted minor version.
    pub minimum_minor: u16,
    /// Highest admitted minor version.
    pub maximum_minor: u16,
    /// Capabilities implemented by the client.
    pub capabilities: ProtocolCapabilities,
    /// Capabilities without which the client will not continue.
    pub required_capabilities: ProtocolCapabilities,
    /// Maximum accepted frame payload.
    pub maximum_frame_payload: u32,
    /// Maximum requests active on the connection.
    pub maximum_in_flight: u32,
    /// Initial per-stream response byte window.
    pub initial_window: u32,
    /// Stable bounded client identity.
    pub client_identity: String,
    /// Requested database.
    pub database: String,
    /// Requested schema.
    pub schema: String,
}

impl Default for Hello {
    fn default() -> Self {
        Self {
            minimum_major: PROTOCOL_MAJOR,
            maximum_major: PROTOCOL_MAJOR,
            minimum_minor: 0,
            maximum_minor: PROTOCOL_MINOR,
            capabilities: ProtocolCapabilities::G6,
            required_capabilities: ProtocolCapabilities::G6,
            maximum_frame_payload: 16 * 1024 * 1024,
            maximum_in_flight: 64,
            initial_window: 64 * 1024,
            client_identity: "hyphae-client".to_owned(),
            database: "main".to_owned(),
            schema: "public".to_owned(),
        }
    }
}

/// Decoded managed `HELLO` with an ephemeral redacted credential.
pub struct AuthenticatedHello {
    hello: Hello,
    credential: ApiKeyCredential,
}

impl AuthenticatedHello {
    /// Returns the non-secret handshake envelope.
    pub const fn hello(&self) -> &Hello {
        &self.hello
    }

    /// Transfers the handshake and credential to the transport adapter.
    pub fn into_parts(self) -> (Hello, ApiKeyCredential) {
        (self.hello, self.credential)
    }
}

impl std::fmt::Debug for AuthenticatedHello {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthenticatedHello")
            .field("hello", &self.hello)
            .field("credential", &"[REDACTED]")
            .finish()
    }
}

/// Server-selected handshake result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Welcome {
    /// Selected protocol major.
    pub major: u16,
    /// Selected protocol minor.
    pub minor: u16,
    /// Negotiated capabilities.
    pub capabilities: ProtocolCapabilities,
    /// Product-service session identity.
    pub session_id: u128,
    /// Selected maximum frame payload.
    pub maximum_frame_payload: u32,
    /// Selected active-request bound.
    pub maximum_in_flight: u32,
    /// Selected initial stream window.
    pub initial_window: u32,
    /// Product contract version.
    pub product_api_version: u16,
    /// Native directory format.
    pub native_directory_format: u16,
    /// Logical catalog codec version.
    pub logical_catalog_codec_version: u16,
    /// Catalog tree format version.
    pub catalog_tree_format_version: u16,
    /// Current catalog version, or zero for an unavailable value.
    pub catalog_version: u64,
    /// Product maximum SQL statement bytes.
    pub max_sql_statement_bytes: u64,
    /// Product maximum SQL parameters.
    pub max_sql_parameters: u64,
    /// Product maximum SQL rows.
    pub max_sql_rows: u64,
}

/// Server-side handshake policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NegotiationPolicy {
    /// Capabilities implemented by the server.
    pub capabilities: ProtocolCapabilities,
    /// Maximum frame payload.
    pub maximum_frame_payload: u32,
    /// Maximum active requests per connection.
    pub maximum_in_flight: u32,
    /// Maximum initial stream window.
    pub maximum_initial_window: u32,
}

impl Default for NegotiationPolicy {
    fn default() -> Self {
        Self {
            capabilities: ProtocolCapabilities::G6,
            maximum_frame_payload: 16 * 1024 * 1024,
            maximum_in_flight: 64,
            maximum_initial_window: 1024 * 1024,
        }
    }
}

/// Handshake codec or negotiation failure.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum HandshakeError {
    /// Input ends before a declared field.
    #[error("native protocol handshake is truncated")]
    Truncated,
    /// Magic, length, reserved bytes, or UTF-8 is invalid.
    #[error("native protocol handshake is malformed")]
    Malformed,
    /// A text or numeric bound is invalid.
    #[error("native protocol handshake exceeds a bound")]
    InvalidLimit,
    /// No supported major/minor pair intersects.
    #[error("native protocol versions are incompatible")]
    IncompatibleVersion,
    /// A required client capability is unavailable.
    #[error("native protocol required capability is unavailable")]
    MissingCapability,
}

/// Encodes one canonical `HELLO` payload.
pub fn encode_hello(hello: &Hello) -> Result<Vec<u8>, HandshakeError> {
    validate_hello(hello)?;
    let client = hello.client_identity.as_bytes();
    let database = hello.database.as_bytes();
    let schema = hello.schema.as_bytes();
    let total = HELLO_HEADER_SIZE
        .checked_add(client.len())
        .and_then(|value| value.checked_add(database.len()))
        .and_then(|value| value.checked_add(schema.len()))
        .ok_or(HandshakeError::InvalidLimit)?;
    let total_u32 = u32::try_from(total).map_err(|_| HandshakeError::InvalidLimit)?;
    let mut encoded = Vec::with_capacity(total);
    encoded.extend_from_slice(HELLO_MAGIC);
    encoded.extend_from_slice(&total_u32.to_le_bytes());
    encoded.extend_from_slice(&hello.minimum_major.to_le_bytes());
    encoded.extend_from_slice(&hello.maximum_major.to_le_bytes());
    encoded.extend_from_slice(&hello.minimum_minor.to_le_bytes());
    encoded.extend_from_slice(&hello.maximum_minor.to_le_bytes());
    encoded.extend_from_slice(&hello.capabilities.bits().to_le_bytes());
    encoded.extend_from_slice(&hello.required_capabilities.bits().to_le_bytes());
    encoded.extend_from_slice(&hello.maximum_frame_payload.to_le_bytes());
    encoded.extend_from_slice(&hello.maximum_in_flight.to_le_bytes());
    encoded.extend_from_slice(&hello.initial_window.to_le_bytes());
    encoded.push(COMPRESSION_NONE);
    encoded.extend_from_slice(&[0; 3]);
    put_u16_len(&mut encoded, client.len())?;
    put_u16_len(&mut encoded, database.len())?;
    put_u16_len(&mut encoded, schema.len())?;
    encoded.extend_from_slice(client);
    encoded.extend_from_slice(database);
    encoded.extend_from_slice(schema);
    Ok(encoded)
}

/// Encodes one canonical managed `HELLO` payload with an API-key trailer.
pub fn encode_authenticated_hello(hello: &Hello, api_key: &str) -> Result<Vec<u8>, HandshakeError> {
    validate_hello(hello)?;
    validate_authenticated_capabilities(hello)?;
    let authentication = api_key.as_bytes();
    if authentication.len() != API_KEY_AUTH_TRAILER_BYTES {
        return Err(HandshakeError::InvalidLimit);
    }
    let client = hello.client_identity.as_bytes();
    let database = hello.database.as_bytes();
    let schema = hello.schema.as_bytes();
    let total = HELLO_HEADER_SIZE
        .checked_add(client.len())
        .and_then(|value| value.checked_add(database.len()))
        .and_then(|value| value.checked_add(schema.len()))
        .and_then(|value| value.checked_add(authentication.len()))
        .ok_or(HandshakeError::InvalidLimit)?;
    let total_u32 = u32::try_from(total).map_err(|_| HandshakeError::InvalidLimit)?;
    let mut encoded = Vec::with_capacity(total);
    encoded.extend_from_slice(HELLO_MAGIC);
    encoded.extend_from_slice(&total_u32.to_le_bytes());
    encoded.extend_from_slice(&hello.minimum_major.to_le_bytes());
    encoded.extend_from_slice(&hello.maximum_major.to_le_bytes());
    encoded.extend_from_slice(&hello.minimum_minor.to_le_bytes());
    encoded.extend_from_slice(&hello.maximum_minor.to_le_bytes());
    encoded.extend_from_slice(&hello.capabilities.bits().to_le_bytes());
    encoded.extend_from_slice(&hello.required_capabilities.bits().to_le_bytes());
    encoded.extend_from_slice(&hello.maximum_frame_payload.to_le_bytes());
    encoded.extend_from_slice(&hello.maximum_in_flight.to_le_bytes());
    encoded.extend_from_slice(&hello.initial_window.to_le_bytes());
    encoded.push(COMPRESSION_NONE);
    encoded.push(AUTHENTICATION_API_KEY);
    put_u16_len(&mut encoded, authentication.len())?;
    put_u16_len(&mut encoded, client.len())?;
    put_u16_len(&mut encoded, database.len())?;
    put_u16_len(&mut encoded, schema.len())?;
    encoded.extend_from_slice(client);
    encoded.extend_from_slice(database);
    encoded.extend_from_slice(schema);
    encoded.extend_from_slice(authentication);
    Ok(encoded)
}

/// Decodes one exact canonical `HELLO` payload.
pub fn decode_hello(encoded: &[u8]) -> Result<Hello, HandshakeError> {
    if encoded.len() < HELLO_HEADER_SIZE {
        return Err(HandshakeError::Truncated);
    }
    if &encoded[..8] != HELLO_MAGIC
        || read_u32(&encoded[8..12]) as usize != encoded.len()
        || encoded[48] != COMPRESSION_NONE
        || encoded[49..52] != [0; 3]
    {
        return Err(HandshakeError::Malformed);
    }
    let client_length = usize::from(read_u16(&encoded[52..54]));
    let database_length = usize::from(read_u16(&encoded[54..56]));
    let schema_length = usize::from(read_u16(&encoded[56..58]));
    let text_length = client_length
        .checked_add(database_length)
        .and_then(|value| value.checked_add(schema_length))
        .ok_or(HandshakeError::InvalidLimit)?;
    if text_length > MAX_HANDSHAKE_TEXT_BYTES || HELLO_HEADER_SIZE + text_length != encoded.len() {
        return Err(HandshakeError::Malformed);
    }
    let client_end = HELLO_HEADER_SIZE + client_length;
    let database_end = client_end + database_length;
    let hello = Hello {
        minimum_major: read_u16(&encoded[12..14]),
        maximum_major: read_u16(&encoded[14..16]),
        minimum_minor: read_u16(&encoded[16..18]),
        maximum_minor: read_u16(&encoded[18..20]),
        capabilities: ProtocolCapabilities::from_bits(read_u64(&encoded[20..28]))
            .ok_or(HandshakeError::Malformed)?,
        required_capabilities: ProtocolCapabilities::from_bits(read_u64(&encoded[28..36]))
            .ok_or(HandshakeError::Malformed)?,
        maximum_frame_payload: read_u32(&encoded[36..40]),
        maximum_in_flight: read_u32(&encoded[40..44]),
        initial_window: read_u32(&encoded[44..48]),
        client_identity: text(&encoded[HELLO_HEADER_SIZE..client_end])?,
        database: text(&encoded[client_end..database_end])?,
        schema: text(&encoded[database_end..])?,
    };
    validate_hello(&hello)?;
    Ok(hello)
}

/// Decodes one exact managed `HELLO` payload and redacts its API key.
pub fn decode_authenticated_hello(encoded: &[u8]) -> Result<AuthenticatedHello, HandshakeError> {
    if encoded.len() < HELLO_HEADER_SIZE {
        return Err(HandshakeError::Truncated);
    }
    let declared_total = read_u32(&encoded[8..12]) as usize;
    if declared_total > encoded.len() {
        return Err(HandshakeError::Truncated);
    }
    if &encoded[..8] != HELLO_MAGIC
        || declared_total != encoded.len()
        || encoded[48] != COMPRESSION_NONE
        || encoded[49] != AUTHENTICATION_API_KEY
    {
        return Err(HandshakeError::Malformed);
    }
    let authentication_length = usize::from(read_u16(&encoded[50..52]));
    if authentication_length != API_KEY_AUTH_TRAILER_BYTES {
        return Err(HandshakeError::InvalidLimit);
    }
    let client_length = usize::from(read_u16(&encoded[52..54]));
    let database_length = usize::from(read_u16(&encoded[54..56]));
    let schema_length = usize::from(read_u16(&encoded[56..58]));
    let text_length = client_length
        .checked_add(database_length)
        .and_then(|value| value.checked_add(schema_length))
        .ok_or(HandshakeError::InvalidLimit)?;
    if text_length > MAX_HANDSHAKE_TEXT_BYTES {
        return Err(HandshakeError::InvalidLimit);
    }
    let expected_total = HELLO_HEADER_SIZE
        .checked_add(text_length)
        .and_then(|value| value.checked_add(authentication_length))
        .ok_or(HandshakeError::InvalidLimit)?;
    if expected_total > encoded.len() {
        return Err(HandshakeError::Truncated);
    }
    if expected_total != encoded.len() {
        return Err(HandshakeError::Malformed);
    }
    let client_end = HELLO_HEADER_SIZE + client_length;
    let database_end = client_end + database_length;
    let schema_end = database_end + schema_length;
    let hello = Hello {
        minimum_major: read_u16(&encoded[12..14]),
        maximum_major: read_u16(&encoded[14..16]),
        minimum_minor: read_u16(&encoded[16..18]),
        maximum_minor: read_u16(&encoded[18..20]),
        capabilities: ProtocolCapabilities::from_bits(read_u64(&encoded[20..28]))
            .ok_or(HandshakeError::Malformed)?,
        required_capabilities: ProtocolCapabilities::from_bits(read_u64(&encoded[28..36]))
            .ok_or(HandshakeError::Malformed)?,
        maximum_frame_payload: read_u32(&encoded[36..40]),
        maximum_in_flight: read_u32(&encoded[40..44]),
        initial_window: read_u32(&encoded[44..48]),
        client_identity: text(&encoded[HELLO_HEADER_SIZE..client_end])?,
        database: text(&encoded[client_end..database_end])?,
        schema: text(&encoded[database_end..schema_end])?,
    };
    validate_hello(&hello)?;
    validate_authenticated_capabilities(&hello)?;
    let api_key =
        std::str::from_utf8(&encoded[schema_end..]).map_err(|_| HandshakeError::Malformed)?;
    let credential = ApiKeyCredential::new(api_key).map_err(|_| HandshakeError::InvalidLimit)?;
    Ok(AuthenticatedHello { hello, credential })
}

/// Selects one complete server handshake.
pub fn negotiate(
    hello: &Hello,
    policy: NegotiationPolicy,
    session_id: u128,
    product: ProductCapabilities,
    catalog_version: u64,
) -> Result<Welcome, HandshakeError> {
    validate_hello(hello)?;
    if session_id == 0
        || policy.maximum_frame_payload == 0
        || policy.maximum_in_flight == 0
        || policy.maximum_initial_window == 0
    {
        return Err(HandshakeError::InvalidLimit);
    }
    if !(hello.minimum_major..=hello.maximum_major).contains(&PROTOCOL_MAJOR) {
        return Err(HandshakeError::IncompatibleVersion);
    }
    let minor = negotiate_minor(PROTOCOL_MINOR, hello.minimum_minor, hello.maximum_minor)?;
    if !hello.capabilities.contains(hello.required_capabilities)
        || !policy.capabilities.contains(hello.required_capabilities)
    {
        return Err(HandshakeError::MissingCapability);
    }
    let selected = hello.capabilities.intersection(policy.capabilities);
    Ok(Welcome {
        major: PROTOCOL_MAJOR,
        minor,
        capabilities: selected,
        session_id,
        maximum_frame_payload: hello
            .maximum_frame_payload
            .min(policy.maximum_frame_payload),
        maximum_in_flight: hello.maximum_in_flight.min(policy.maximum_in_flight),
        initial_window: hello.initial_window.min(policy.maximum_initial_window),
        product_api_version: product.product_api_version,
        native_directory_format: product.native_directory_format,
        logical_catalog_codec_version: product.logical_catalog_codec_version,
        catalog_tree_format_version: product.catalog_tree_format_version,
        catalog_version,
        max_sql_statement_bytes: to_u64(product.max_sql_statement_bytes)?,
        max_sql_parameters: to_u64(product.max_sql_parameters)?,
        max_sql_rows: to_u64(product.max_sql_rows)?,
    })
}

/// Encodes one canonical `WELCOME` payload.
pub fn encode_welcome(welcome: Welcome) -> Result<Vec<u8>, HandshakeError> {
    if welcome.major == 0
        || welcome.session_id == 0
        || welcome.maximum_frame_payload == 0
        || welcome.maximum_in_flight == 0
        || welcome.initial_window == 0
    {
        return Err(HandshakeError::InvalidLimit);
    }
    let mut encoded = Vec::with_capacity(WELCOME_SIZE);
    encoded.extend_from_slice(WELCOME_MAGIC);
    encoded.extend_from_slice(&94_u32.to_le_bytes());
    encoded.extend_from_slice(&welcome.major.to_le_bytes());
    encoded.extend_from_slice(&welcome.minor.to_le_bytes());
    encoded.extend_from_slice(&welcome.capabilities.bits().to_le_bytes());
    encoded.extend_from_slice(&welcome.session_id.to_le_bytes());
    encoded.extend_from_slice(&welcome.maximum_frame_payload.to_le_bytes());
    encoded.extend_from_slice(&welcome.maximum_in_flight.to_le_bytes());
    encoded.extend_from_slice(&welcome.initial_window.to_le_bytes());
    encoded.extend_from_slice(&welcome.product_api_version.to_le_bytes());
    encoded.extend_from_slice(&welcome.native_directory_format.to_le_bytes());
    encoded.extend_from_slice(&welcome.logical_catalog_codec_version.to_le_bytes());
    encoded.extend_from_slice(&welcome.catalog_tree_format_version.to_le_bytes());
    encoded.extend_from_slice(&welcome.catalog_version.to_le_bytes());
    encoded.extend_from_slice(&welcome.max_sql_statement_bytes.to_le_bytes());
    encoded.extend_from_slice(&welcome.max_sql_parameters.to_le_bytes());
    encoded.extend_from_slice(&welcome.max_sql_rows.to_le_bytes());
    encoded.extend_from_slice(&0_u16.to_le_bytes());
    Ok(encoded)
}

/// Decodes one exact canonical `WELCOME` payload.
pub fn decode_welcome(encoded: &[u8]) -> Result<Welcome, HandshakeError> {
    if encoded.len() < WELCOME_SIZE {
        return Err(HandshakeError::Truncated);
    }
    if encoded.len() != WELCOME_SIZE
        || &encoded[..8] != WELCOME_MAGIC
        || read_u32(&encoded[8..12]) as usize != WELCOME_SIZE
        || read_u16(&encoded[92..94]) != 0
    {
        return Err(HandshakeError::Malformed);
    }
    let welcome = Welcome {
        major: read_u16(&encoded[12..14]),
        minor: read_u16(&encoded[14..16]),
        capabilities: ProtocolCapabilities::from_bits(read_u64(&encoded[16..24]))
            .ok_or(HandshakeError::Malformed)?,
        session_id: read_u128(&encoded[24..40]),
        maximum_frame_payload: read_u32(&encoded[40..44]),
        maximum_in_flight: read_u32(&encoded[44..48]),
        initial_window: read_u32(&encoded[48..52]),
        product_api_version: read_u16(&encoded[52..54]),
        native_directory_format: read_u16(&encoded[54..56]),
        logical_catalog_codec_version: read_u16(&encoded[56..58]),
        catalog_tree_format_version: read_u16(&encoded[58..60]),
        catalog_version: read_u64(&encoded[60..68]),
        max_sql_statement_bytes: read_u64(&encoded[68..76]),
        max_sql_parameters: read_u64(&encoded[76..84]),
        max_sql_rows: read_u64(&encoded[84..92]),
    };
    if welcome.major == 0
        || welcome.session_id == 0
        || welcome.maximum_frame_payload == 0
        || welcome.maximum_in_flight == 0
        || welcome.initial_window == 0
    {
        return Err(HandshakeError::InvalidLimit);
    }
    Ok(welcome)
}

fn validate_hello(hello: &Hello) -> Result<(), HandshakeError> {
    let text_bytes = hello
        .client_identity
        .len()
        .checked_add(hello.database.len())
        .and_then(|value| value.checked_add(hello.schema.len()))
        .ok_or(HandshakeError::InvalidLimit)?;
    if hello.minimum_major == 0
        || hello.minimum_major > hello.maximum_major
        || hello.minimum_minor > hello.maximum_minor
        || !hello.capabilities.contains(hello.required_capabilities)
        || hello.maximum_frame_payload == 0
        || hello.maximum_frame_payload as usize > crate::DEFAULT_MAX_FRAME_PAYLOAD
        || hello.maximum_in_flight == 0
        || hello.initial_window == 0
        || hello.client_identity.is_empty()
        || hello.database.is_empty()
        || hello.schema.is_empty()
        || text_bytes > MAX_HANDSHAKE_TEXT_BYTES
    {
        return Err(HandshakeError::InvalidLimit);
    }
    Ok(())
}

fn validate_authenticated_capabilities(hello: &Hello) -> Result<(), HandshakeError> {
    if hello
        .capabilities
        .contains(ProtocolCapabilities::API_KEY_AUTH)
        && hello
            .required_capabilities
            .contains(ProtocolCapabilities::API_KEY_AUTH)
    {
        Ok(())
    } else {
        Err(HandshakeError::MissingCapability)
    }
}

fn negotiate_minor(
    server_maximum_minor: u16,
    client_minimum_minor: u16,
    client_maximum_minor: u16,
) -> Result<u16, HandshakeError> {
    let selected = server_maximum_minor.min(client_maximum_minor);
    if client_minimum_minor > selected {
        Err(HandshakeError::IncompatibleVersion)
    } else {
        Ok(selected)
    }
}

fn put_u16_len(encoded: &mut Vec<u8>, length: usize) -> Result<(), HandshakeError> {
    encoded.extend_from_slice(
        &u16::try_from(length)
            .map_err(|_| HandshakeError::InvalidLimit)?
            .to_le_bytes(),
    );
    Ok(())
}

fn text(encoded: &[u8]) -> Result<String, HandshakeError> {
    std::str::from_utf8(encoded)
        .map(str::to_owned)
        .map_err(|_| HandshakeError::Malformed)
}

fn to_u64(value: usize) -> Result<u64, HandshakeError> {
    u64::try_from(value).map_err(|_| HandshakeError::InvalidLimit)
}

fn read_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(bytes.try_into().unwrap_or([0; 2]))
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().unwrap_or([0; 4]))
}

fn read_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().unwrap_or([0; 8]))
}

fn read_u128(bytes: &[u8]) -> u128 {
    u128::from_le_bytes(bytes.try_into().unwrap_or([0; 16]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minor_negotiation_preserves_a_legacy_client_ceiling() {
        assert_eq!(negotiate_minor(1, 0, 0), Ok(0));
    }

    #[test]
    fn minor_negotiation_selects_the_highest_common_minor() {
        assert_eq!(negotiate_minor(2, 1, 3), Ok(2));
    }

    #[test]
    fn minor_negotiation_rejects_disjoint_intervals() {
        assert_eq!(
            negotiate_minor(0, 1, 1),
            Err(HandshakeError::IncompatibleVersion)
        );
    }
}
