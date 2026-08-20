// SPDX-License-Identifier: Apache-2.0

//! Bounded LZF decompression for RDB compressed strings.

use thiserror::Error;

/// Failure while decompressing one bounded LZF payload.
#[derive(Debug, Error, Eq, PartialEq)]
pub(crate) enum LzfError {
    /// The compressed stream ended inside a token.
    #[error("LZF input is truncated")]
    Truncated,
    /// A back-reference pointed before the start of the output.
    #[error("LZF back-reference is out of range")]
    BackReference,
    /// The decompressed output did not match the declared length.
    #[error("LZF output length {actual} differs from the declared {declared}")]
    Length {
        /// Produced output length.
        actual: usize,
        /// Declared output length.
        declared: usize,
    },
    /// The declared output length exceeded the configured bound.
    #[error("LZF declared length {declared} exceeds the bound {maximum}")]
    Bound {
        /// Declared output length.
        declared: usize,
        /// Maximum admitted output length.
        maximum: usize,
    },
}

/// Decompresses one LZF payload into exactly `declared_len` bytes.
pub(crate) fn decompress(
    input: &[u8],
    declared_len: usize,
    maximum: usize,
) -> Result<Vec<u8>, LzfError> {
    if declared_len > maximum {
        return Err(LzfError::Bound {
            declared: declared_len,
            maximum,
        });
    }
    let mut output = Vec::with_capacity(declared_len);
    let mut position = 0_usize;
    while position < input.len() {
        let control = usize::from(input[position]);
        position += 1;
        if control < 32 {
            let run = control + 1;
            let end = position.checked_add(run).ok_or(LzfError::Truncated)?;
            if end > input.len() || output.len() + run > declared_len {
                return Err(LzfError::Truncated);
            }
            output.extend_from_slice(&input[position..end]);
            position = end;
        } else {
            let mut length = control >> 5;
            if length == 7 {
                let extra = *input.get(position).ok_or(LzfError::Truncated)?;
                length += usize::from(extra);
                position += 1;
            }
            let low = *input.get(position).ok_or(LzfError::Truncated)?;
            position += 1;
            let distance = ((control & 0x1f) << 8) + usize::from(low) + 1;
            let start = output
                .len()
                .checked_sub(distance)
                .ok_or(LzfError::BackReference)?;
            let copy = length + 2;
            if output.len() + copy > declared_len {
                return Err(LzfError::Length {
                    actual: output.len() + copy,
                    declared: declared_len,
                });
            }
            for offset in 0..copy {
                let byte = output[start + offset];
                output.push(byte);
            }
        }
    }
    if output.len() == declared_len {
        Ok(output)
    } else {
        Err(LzfError::Length {
            actual: output.len(),
            declared: declared_len,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{LzfError, decompress};

    #[test]
    fn literal_runs_round_trip() -> Result<(), LzfError> {
        // Control byte 4 = literal run of 5 bytes.
        let input = [4, b'a', b'b', b'c', b'd', b'e'];
        assert_eq!(decompress(&input, 5, 1024)?, b"abcde");
        Ok(())
    }

    #[test]
    fn back_references_expand_repeats() -> Result<(), LzfError> {
        // "abc" literal, then a back-reference of length 3+2=5 at distance 3
        // expands "abcabcab".
        let input = [2, b'a', b'b', b'c', 0b0110_0000, 2];
        assert_eq!(decompress(&input, 8, 1024)?, b"abcabcab");
        Ok(())
    }

    #[test]
    fn truncation_and_bad_references_fail_closed() {
        assert_eq!(decompress(&[4, b'a'], 5, 1024), Err(LzfError::Truncated));
        assert!(matches!(
            decompress(&[0b0110_0000, 9], 5, 1024),
            Err(LzfError::BackReference)
        ));
        assert!(matches!(
            decompress(&[0], 10, 4),
            Err(LzfError::Bound { .. })
        ));
    }
}
