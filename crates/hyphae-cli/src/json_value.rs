// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use hyphae_query::Value;
use thiserror::Error;

/// Failure converting CLI JSON into deterministic structured values.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub(crate) enum JsonValueError {
    #[error("invalid JSON: {0}")]
    Syntax(String),

    #[error("structured query accepts only signed 64-bit integer JSON numbers")]
    NonIntegerNumber,
}

/// Failure decoding a fixed-length lowercase or uppercase hexadecimal value.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub(crate) enum HexError {
    #[error("hex value must contain exactly {expected} characters; found {actual}")]
    Length { expected: usize, actual: usize },

    #[error("hex value contains a non-hexadecimal character")]
    Character,
}

pub(crate) fn parse_json(input: &str) -> Result<Value, JsonValueError> {
    let value =
        serde_json::from_str(input).map_err(|source| JsonValueError::Syntax(source.to_string()))?;
    from_json(value)
}

pub(crate) fn from_json(value: serde_json::Value) -> Result<Value, JsonValueError> {
    match value {
        serde_json::Value::Null => Ok(Value::Null),
        serde_json::Value::Bool(value) => Ok(Value::Boolean(value)),
        serde_json::Value::Number(value) => value
            .as_i64()
            .map(Value::Integer)
            .ok_or(JsonValueError::NonIntegerNumber),
        serde_json::Value::String(value) => Ok(Value::String(value)),
        serde_json::Value::Array(values) => values
            .into_iter()
            .map(from_json)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        serde_json::Value::Object(values) => values
            .into_iter()
            .map(|(key, value)| Ok((key, from_json(value)?)))
            .collect::<Result<BTreeMap<_, _>, _>>()
            .map(Value::Object),
    }
}

pub(crate) fn to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::Null => serde_json::Value::Null,
        Value::Boolean(value) => serde_json::Value::Bool(*value),
        Value::Integer(value) => serde_json::Value::Number((*value).into()),
        Value::String(value) => serde_json::Value::String(value.clone()),
        Value::Bytes(value) => serde_json::json!({
            "$hyphae_bytes_hex": encode_hex(value),
        }),
        Value::Array(values) => serde_json::Value::Array(values.iter().map(to_json).collect()),
        Value::Object(values) => serde_json::Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), to_json(value)))
                .collect(),
        ),
    }
}

pub(crate) fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

pub(crate) fn decode_hex<const N: usize>(encoded: &str) -> Result<[u8; N], HexError> {
    let encoded = encoded.trim();
    let expected = N.saturating_mul(2);
    if encoded.len() != expected {
        return Err(HexError::Length {
            expected,
            actual: encoded.len(),
        });
    }
    let mut decoded = [0_u8; N];
    for (index, pair) in encoded.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_nibble(pair[0]).ok_or(HexError::Character)?;
        let low = hex_nibble(pair[1]).ok_or(HexError::Character)?;
        decoded[index] = (high << 4) | low;
    }
    Ok(decoded)
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use hyphae_query::Value;

    use super::{HexError, JsonValueError, decode_hex, encode_hex, parse_json, to_json};

    #[test]
    fn json_round_trips_the_cli_subset() -> Result<(), JsonValueError> {
        let value = parse_json(r#"{"enabled":true,"items":[1,null,"x"]}"#)?;
        assert_eq!(parse_json(&to_json(&value).to_string())?, value);
        assert_eq!(encode_hex(&[0, 15, 16, 255]), "000f10ff");
        Ok(())
    }

    #[test]
    fn floating_point_json_is_rejected() {
        assert_eq!(parse_json("1.5"), Err(JsonValueError::NonIntegerNumber));
        assert_eq!(
            to_json(&Value::Bytes(vec![1, 2])),
            serde_json::json!({"$hyphae_bytes_hex": "0102"})
        );
    }

    #[test]
    fn fixed_hex_round_trips_and_rejects_bad_input() {
        assert_eq!(decode_hex::<4>("000f10FF"), Ok([0, 15, 16, 255]));
        assert_eq!(decode_hex::<2>(" 00ff\r\n"), Ok([0, 255]));
        assert_eq!(
            decode_hex::<4>("00"),
            Err(HexError::Length {
                expected: 8,
                actual: 2
            })
        );
        assert_eq!(decode_hex::<1>("xz"), Err(HexError::Character));
    }
}
