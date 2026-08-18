// SPDX-License-Identifier: Apache-2.0

//! Canonical bounded binary codec for [`crate::ProductError`].

use std::fmt;

use hyphae_native_types::{ObjectId, TransactionId};

use crate::error::{
    MAX_PRODUCT_ERROR_DETAIL_BYTES, MAX_PRODUCT_ERROR_IDENTIFIER_BYTES,
    MAX_PRODUCT_ERROR_MESSAGE_BYTES, MAX_PRODUCT_ERROR_UNKNOWN_DETAILS, ProductError,
    ProductErrorCategory, ProductErrorCode, ProductErrorDetails, ProductErrorValidationError,
    ProductLimit, ProductLimitKind, ProductRetry, ProductSourceSpan, ProductSqlSubcode,
    ProductTransactionState, ProductUnknownDetail,
};

/// Exact v1 product-error envelope magic.
pub const PRODUCT_ERROR_CODEC_MAGIC: [u8; 8] = *b"HYPERR01";
/// Maximum complete encoded product-error envelope bytes.
pub const MAX_ENCODED_PRODUCT_ERROR_BYTES: usize = 8 * 1024;

const HEADER_SIZE: usize = 20;
const FLAG_REQUEST_ID: u8 = 1 << 0;
const FLAG_TRACE_ID: u8 = 1 << 1;
const FLAG_OBJECT_ID: u8 = 1 << 2;
const FLAG_LIMIT: u8 = 1 << 3;
const FLAG_SOURCE_SPAN: u8 = 1 << 4;
const KNOWN_FLAGS: u8 =
    FLAG_REQUEST_ID | FLAG_TRACE_ID | FLAG_OBJECT_ID | FLAG_LIMIT | FLAG_SOURCE_SPAN;
const DETAIL_SQL_SUBCODE: u16 = 1;
const DETAIL_TRANSACTION_ID: u16 = 2;

/// Canonical product-error envelope codec failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProductErrorCodecError {
    /// The input ends before a declared field is complete.
    Truncated,
    /// The complete envelope exceeds the fixed v1 bound.
    EnvelopeTooLarge,
    /// The magic or declared total length is invalid.
    InvalidEnvelope,
    /// A reserved flag or unknown enum discriminant is present.
    UnsupportedField,
    /// Text is not valid UTF-8.
    InvalidUtf8,
    /// An identity is zero or otherwise invalid.
    InvalidIdentity,
    /// A bounded field exceeds its v1 limit.
    FieldTooLarge,
    /// Fields are duplicated, unsorted, reserved, or otherwise noncanonical.
    Noncanonical,
    /// Registered code fields or transaction fields are inconsistent.
    InconsistentFields,
}

impl fmt::Display for ProductErrorCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Truncated => "product error envelope is truncated",
            Self::EnvelopeTooLarge => "product error envelope is too large",
            Self::InvalidEnvelope => "product error envelope header is invalid",
            Self::UnsupportedField => "product error envelope contains an unsupported field",
            Self::InvalidUtf8 => "product error envelope contains invalid UTF-8",
            Self::InvalidIdentity => "product error envelope contains an invalid identity",
            Self::FieldTooLarge => "product error envelope field is too large",
            Self::Noncanonical => "product error envelope is noncanonical",
            Self::InconsistentFields => "product error envelope fields are inconsistent",
        })
    }
}

impl std::error::Error for ProductErrorCodecError {}

/// Encodes one canonical bounded `HYPERR01` envelope.
///
/// # Errors
///
/// Returns an error when the error has inconsistent transaction fields or the
/// complete envelope exceeds the fixed bound.
pub fn encode_product_error(error: &ProductError) -> Result<Vec<u8>, ProductErrorCodecError> {
    error
        .validate_transaction_fields()
        .map_err(map_validation_error)?;

    let code_value = error.code();
    let code = code_value.as_str().as_bytes();
    let message = error.message().as_bytes();
    let code_length =
        u8::try_from(code.len()).map_err(|_| ProductErrorCodecError::FieldTooLarge)?;
    let message_length =
        u16::try_from(message.len()).map_err(|_| ProductErrorCodecError::FieldTooLarge)?;
    let detail_count = u8::try_from(error.details().field_count())
        .map_err(|_| ProductErrorCodecError::FieldTooLarge)?;

    let mut flags = 0_u8;
    flags |= error.request_id().map_or(0, |_| FLAG_REQUEST_ID);
    flags |= error.trace_id().map_or(0, |_| FLAG_TRACE_ID);
    flags |= error.object_id().map_or(0, |_| FLAG_OBJECT_ID);
    flags |= error.limit().map_or(0, |_| FLAG_LIMIT);
    flags |= error.source_span().map_or(0, |_| FLAG_SOURCE_SPAN);

    let mut encoded = Vec::with_capacity(HEADER_SIZE + code.len() + message.len() + 128);
    encoded.extend_from_slice(&PRODUCT_ERROR_CODEC_MAGIC);
    encoded.extend_from_slice(&0_u32.to_le_bytes());
    encoded.push(error.category().wire_tag());
    encoded.push(error.retry().wire_tag());
    encoded.push(error.transaction_state().wire_tag());
    encoded.push(flags);
    encoded.push(code_length);
    encoded.extend_from_slice(&message_length.to_le_bytes());
    encoded.push(detail_count);
    encoded.extend_from_slice(code);
    encoded.extend_from_slice(message);

    if let Some(request_id) = error.request_id() {
        encoded.extend_from_slice(&request_id.to_le_bytes());
    }
    if let Some(trace_id) = error.trace_id() {
        encoded.extend_from_slice(&trace_id.to_le_bytes());
    }
    if let Some(object_id) = error.object_id() {
        encoded.extend_from_slice(&object_id.get().to_le_bytes());
    }
    if let Some(limit) = error.limit() {
        let kind_value = limit.kind();
        let kind = kind_value.as_str().as_bytes();
        encoded.push(u8::try_from(kind.len()).map_err(|_| ProductErrorCodecError::FieldTooLarge)?);
        encoded.extend_from_slice(kind);
        encoded.extend_from_slice(&limit.configured().to_le_bytes());
        encoded.extend_from_slice(&limit.observed().to_le_bytes());
    }
    if let Some(span) = error.source_span() {
        encoded.extend_from_slice(&span.start().to_le_bytes());
        encoded.extend_from_slice(&span.end().to_le_bytes());
    }

    if let Some(subcode) = error.details().sql_subcode() {
        encode_detail(
            &mut encoded,
            DETAIL_SQL_SUBCODE,
            subcode.as_str().as_bytes(),
        )?;
    }
    if let Some(transaction_id) = error.details().transaction_id() {
        encode_detail(
            &mut encoded,
            DETAIL_TRANSACTION_ID,
            &transaction_id.get().to_le_bytes(),
        )?;
    }
    for detail in error.details().unknown() {
        encode_detail(&mut encoded, detail.tag(), detail.value())?;
    }

    if encoded.len() > MAX_ENCODED_PRODUCT_ERROR_BYTES {
        return Err(ProductErrorCodecError::EnvelopeTooLarge);
    }
    let total_length =
        u32::try_from(encoded.len()).map_err(|_| ProductErrorCodecError::EnvelopeTooLarge)?;
    encoded[8..12].copy_from_slice(&total_length.to_le_bytes());
    Ok(encoded)
}

fn encode_detail(
    encoded: &mut Vec<u8>,
    tag: u16,
    value: &[u8],
) -> Result<(), ProductErrorCodecError> {
    let length = u16::try_from(value.len()).map_err(|_| ProductErrorCodecError::FieldTooLarge)?;
    encoded.extend_from_slice(&tag.to_le_bytes());
    encoded.extend_from_slice(&length.to_le_bytes());
    encoded.extend_from_slice(value);
    Ok(())
}

/// Decodes one exact canonical bounded `HYPERR01` envelope.
///
/// Unknown stable codes and unknown detail TLVs are retained verbatim within
/// their v1 bounds.
///
/// # Errors
///
/// Returns an error for truncation, trailing bytes, invalid discriminants,
/// noncanonical field ordering, unsafe registered-code messages, or a bound
/// violation.
pub fn decode_product_error(encoded: &[u8]) -> Result<ProductError, ProductErrorCodecError> {
    if encoded.len() > MAX_ENCODED_PRODUCT_ERROR_BYTES {
        return Err(ProductErrorCodecError::EnvelopeTooLarge);
    }
    if encoded.len() < HEADER_SIZE {
        return Err(ProductErrorCodecError::Truncated);
    }
    if encoded[..8] != PRODUCT_ERROR_CODEC_MAGIC {
        return Err(ProductErrorCodecError::InvalidEnvelope);
    }
    let declared_length = usize::try_from(u32::from_le_bytes(copy_array(&encoded[8..12])))
        .map_err(|_| ProductErrorCodecError::EnvelopeTooLarge)?;
    if declared_length != encoded.len() {
        return Err(ProductErrorCodecError::InvalidEnvelope);
    }

    let category = ProductErrorCategory::from_wire_tag(encoded[12])
        .ok_or(ProductErrorCodecError::UnsupportedField)?;
    let retry =
        ProductRetry::from_wire_tag(encoded[13]).ok_or(ProductErrorCodecError::UnsupportedField)?;
    let transaction_state = ProductTransactionState::from_wire_tag(encoded[14])
        .ok_or(ProductErrorCodecError::UnsupportedField)?;
    let flags = encoded[15];
    if flags & !KNOWN_FLAGS != 0 {
        return Err(ProductErrorCodecError::UnsupportedField);
    }
    let code_length = usize::from(encoded[16]);
    let message_length = usize::from(u16::from_le_bytes(copy_array(&encoded[17..19])));
    let detail_count = usize::from(encoded[19]);
    if code_length == 0
        || code_length > MAX_PRODUCT_ERROR_IDENTIFIER_BYTES
        || message_length == 0
        || message_length > MAX_PRODUCT_ERROR_MESSAGE_BYTES
        || detail_count > MAX_PRODUCT_ERROR_UNKNOWN_DETAILS + 2
    {
        return Err(ProductErrorCodecError::FieldTooLarge);
    }

    let mut decoder = Decoder::new(&encoded[HEADER_SIZE..]);
    let code_text = decoder.utf8(code_length)?;
    let code = ProductErrorCode::from_raw(code_text).map_err(map_validation_error)?;
    let message = decoder.utf8(message_length)?;
    let known_code = code.is_known();
    let mut error =
        ProductError::try_new(code, category, retry, message).map_err(map_validation_error)?;

    if flags & FLAG_REQUEST_ID != 0 {
        error = error.with_request_id(decoder.u128()?);
    }
    if flags & FLAG_TRACE_ID != 0 {
        error = error.with_trace_id(decoder.u128()?);
    }
    if flags & FLAG_OBJECT_ID != 0 {
        let object_id =
            ObjectId::new(decoder.u128()?).map_err(|_| ProductErrorCodecError::InvalidIdentity)?;
        error = error.with_object_id(object_id);
    }
    if flags & FLAG_LIMIT != 0 {
        let kind_length = usize::from(decoder.u8()?);
        if kind_length == 0 || kind_length > MAX_PRODUCT_ERROR_IDENTIFIER_BYTES {
            return Err(ProductErrorCodecError::FieldTooLarge);
        }
        let kind =
            ProductLimitKind::from_raw(decoder.utf8(kind_length)?).map_err(map_validation_error)?;
        error = error.with_limit(ProductLimit::new(kind, decoder.u64()?, decoder.u64()?));
    }
    if flags & FLAG_SOURCE_SPAN != 0 {
        let span =
            ProductSourceSpan::new(decoder.u32()?, decoder.u32()?).map_err(map_validation_error)?;
        error = error.with_source_span(span);
    }

    let details = decode_details(&mut decoder, detail_count)?;
    if !decoder.is_empty() {
        return Err(ProductErrorCodecError::InvalidEnvelope);
    }
    error.set_details(details);
    error
        .set_transaction_state(transaction_state)
        .map_err(map_validation_error)?;
    if !known_code && encode_product_error(&error)?.as_slice() != encoded {
        return Err(ProductErrorCodecError::Noncanonical);
    }
    Ok(error)
}

fn decode_details(
    decoder: &mut Decoder<'_>,
    detail_count: usize,
) -> Result<ProductErrorDetails, ProductErrorCodecError> {
    let mut details = ProductErrorDetails::default();
    let mut previous_tag = 0_u16;
    for _ in 0..detail_count {
        let tag = decoder.u16()?;
        let length = usize::from(decoder.u16()?);
        if tag == 0 || tag <= previous_tag {
            return Err(ProductErrorCodecError::Noncanonical);
        }
        previous_tag = tag;
        if length > MAX_PRODUCT_ERROR_DETAIL_BYTES {
            return Err(ProductErrorCodecError::FieldTooLarge);
        }
        let value = decoder.bytes(length)?;
        match tag {
            DETAIL_SQL_SUBCODE => {
                if length != 8 {
                    return Err(ProductErrorCodecError::Noncanonical);
                }
                let raw =
                    std::str::from_utf8(value).map_err(|_| ProductErrorCodecError::InvalidUtf8)?;
                details.set_sql_subcode(
                    ProductSqlSubcode::from_raw(raw)
                        .ok_or(ProductErrorCodecError::UnsupportedField)?,
                );
            }
            DETAIL_TRANSACTION_ID => {
                if length != 16 {
                    return Err(ProductErrorCodecError::Noncanonical);
                }
                let transaction_id = TransactionId::new(u128::from_le_bytes(copy_array(value)))
                    .map_err(|_| ProductErrorCodecError::InvalidIdentity)?;
                details.set_transaction_id(transaction_id);
            }
            _ => details
                .insert_unknown(
                    ProductUnknownDetail::new(tag, value).map_err(map_validation_error)?,
                )
                .map_err(map_validation_error)?,
        }
    }
    Ok(details)
}

fn map_validation_error(error: ProductErrorValidationError) -> ProductErrorCodecError {
    match error {
        ProductErrorValidationError::InvalidIdentifier
        | ProductErrorValidationError::InvalidMessage
        | ProductErrorValidationError::InvalidSourceSpan
        | ProductErrorValidationError::ReservedDetailTag
        | ProductErrorValidationError::DuplicateDetail => ProductErrorCodecError::Noncanonical,
        ProductErrorValidationError::DetailTooLarge
        | ProductErrorValidationError::TooManyDetails => ProductErrorCodecError::FieldTooLarge,
        ProductErrorValidationError::InconsistentKnownCode
        | ProductErrorValidationError::InconsistentTransactionState => {
            ProductErrorCodecError::InconsistentFields
        }
    }
}

struct Decoder<'a> {
    remaining: &'a [u8],
}

impl<'a> Decoder<'a> {
    const fn new(remaining: &'a [u8]) -> Self {
        Self { remaining }
    }

    fn bytes(&mut self, length: usize) -> Result<&'a [u8], ProductErrorCodecError> {
        if self.remaining.len() < length {
            return Err(ProductErrorCodecError::Truncated);
        }
        let (value, remaining) = self.remaining.split_at(length);
        self.remaining = remaining;
        Ok(value)
    }

    fn utf8(&mut self, length: usize) -> Result<&'a str, ProductErrorCodecError> {
        std::str::from_utf8(self.bytes(length)?).map_err(|_| ProductErrorCodecError::InvalidUtf8)
    }

    fn u8(&mut self) -> Result<u8, ProductErrorCodecError> {
        Ok(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ProductErrorCodecError> {
        Ok(u16::from_le_bytes(copy_array(self.bytes(2)?)))
    }

    fn u32(&mut self) -> Result<u32, ProductErrorCodecError> {
        Ok(u32::from_le_bytes(copy_array(self.bytes(4)?)))
    }

    fn u64(&mut self) -> Result<u64, ProductErrorCodecError> {
        Ok(u64::from_le_bytes(copy_array(self.bytes(8)?)))
    }

    fn u128(&mut self) -> Result<u128, ProductErrorCodecError> {
        Ok(u128::from_le_bytes(copy_array(self.bytes(16)?)))
    }

    const fn is_empty(&self) -> bool {
        self.remaining.is_empty()
    }
}

fn copy_array<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut array = [0; N];
    array.copy_from_slice(bytes);
    array
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ProductErrorIdentifier, ProductFailureBoundary, ProductLimitKind, ProductSqlSubcode,
    };

    fn canonical_error() -> Result<ProductError, Box<dyn std::error::Error>> {
        let object_id = ObjectId::new(17)?;
        let transaction_id = TransactionId::new(19)?;
        Ok(ProductFailureBoundary::active(transaction_id).apply(
            ProductError::from_code(ProductErrorCode::SqlInvalidSyntax)
                .with_request_id(11)
                .with_trace_id(13)
                .with_object_id(object_id)
                .with_limit(ProductLimit::new(
                    ProductLimitKind::SqlStatementBytes,
                    64,
                    65,
                ))
                .with_source_span(ProductSourceSpan::new(2, 8)?)
                .with_sql_subcode(ProductSqlSubcode::Hysql001)
                .with_unknown_detail(ProductUnknownDetail::new(9, b"opaque")?)?,
        ))
    }

    #[test]
    fn canonical_round_trip_covers_every_field() -> Result<(), Box<dyn std::error::Error>> {
        let expected = canonical_error()?;
        let encoded = encode_product_error(&expected)?;
        assert_eq!(&encoded[..8], b"HYPERR01");
        assert_eq!(decode_product_error(&encoded)?, expected);
        assert_eq!(
            encode_product_error(&decode_product_error(&encoded)?)?,
            encoded
        );
        Ok(())
    }

    #[test]
    fn unknown_code_and_details_survive_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let raw = ProductErrorIdentifier::new("future_failure")?;
        let expected = ProductError::try_new(
            ProductErrorCode::Unknown(raw),
            ProductErrorCategory::Internal,
            ProductRetry::AfterRecovery,
            "future product operation failed",
        )?
        .with_unknown_detail(ProductUnknownDetail::new(42, &[1, 2, 3])?)?;
        let encoded = encode_product_error(&expected)?;
        let decoded = decode_product_error(&encoded)?;
        assert_eq!(decoded, expected);
        assert_eq!(decoded.code().as_str(), "future_failure");
        assert_eq!(decoded.details().unknown()[0].value(), &[1, 2, 3]);
        Ok(())
    }

    #[test]
    fn publication_unknown_round_trips_exact_semantics() -> Result<(), Box<dyn std::error::Error>> {
        let transaction_id = TransactionId::new(99)?;
        let expected = ProductFailureBoundary::publication_unknown(transaction_id)
            .apply(ProductError::from_code(ProductErrorCode::Cancelled));
        let decoded = decode_product_error(&encode_product_error(&expected)?)?;
        assert_eq!(decoded.code(), ProductErrorCode::UnknownCommit);
        assert_eq!(decoded.retry(), ProductRetry::UnknownCommit);
        assert_eq!(
            decoded.transaction_state(),
            ProductTransactionState::OutcomeUnknown
        );
        assert_eq!(decoded.details().transaction_id(), Some(transaction_id));
        Ok(())
    }

    #[test]
    fn decoder_rejects_every_truncation() -> Result<(), Box<dyn std::error::Error>> {
        let encoded = encode_product_error(&canonical_error()?)?;
        for length in 0..encoded.len() {
            assert!(
                decode_product_error(&encoded[..length]).is_err(),
                "length {length}"
            );
        }
        Ok(())
    }

    #[test]
    fn decoder_rejects_size_length_flags_and_noncanonical_details()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            decode_product_error(&vec![0_u8; MAX_ENCODED_PRODUCT_ERROR_BYTES + 1]),
            Err(ProductErrorCodecError::EnvelopeTooLarge)
        );

        let encoded = encode_product_error(&canonical_error()?)?;
        let mut trailing = encoded.clone();
        trailing.push(0);
        let trailing_length = u32::try_from(trailing.len())?;
        trailing[8..12].copy_from_slice(&trailing_length.to_le_bytes());
        assert_eq!(
            decode_product_error(&trailing),
            Err(ProductErrorCodecError::InvalidEnvelope)
        );

        let mut reserved = encoded.clone();
        reserved[15] |= 0x80;
        assert_eq!(
            decode_product_error(&reserved),
            Err(ProductErrorCodecError::UnsupportedField)
        );

        let mut reversed_details = ProductError::from_code(ProductErrorCode::SqlInvalidSyntax)
            .with_sql_subcode(ProductSqlSubcode::Hysql001)
            .with_unknown_detail(ProductUnknownDetail::new(9, b"opaque")?)?;
        let mut reversed = encode_product_error(&reversed_details)?;
        let details_start = reversed.len() - (4 + 8) - (4 + 6);
        let first = reversed[details_start..details_start + 12].to_vec();
        let second = reversed[details_start + 12..].to_vec();
        reversed.truncate(details_start);
        reversed.extend_from_slice(&second);
        reversed.extend_from_slice(&first);
        assert_eq!(
            decode_product_error(&reversed),
            Err(ProductErrorCodecError::Noncanonical)
        );

        reversed_details = ProductError::from_code(ProductErrorCode::Internal);
        let mut wrong_message = encode_product_error(&reversed_details)?;
        let message_offset = HEADER_SIZE + usize::from(wrong_message[16]);
        wrong_message[message_offset] ^= 1;
        assert_eq!(
            decode_product_error(&wrong_message),
            Err(ProductErrorCodecError::InconsistentFields)
        );
        Ok(())
    }
}
