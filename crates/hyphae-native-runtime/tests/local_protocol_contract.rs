// SPDX-License-Identifier: Apache-2.0

//! Frozen native local-protocol v1 frame-kind registry.

use hyphae_native_runtime::{FrameKind, LocalProtocolError, decode_frame, encode_frame};

#[test]
fn all_v1_frame_kind_codes_are_frozen_and_decodeable() -> Result<(), Box<dyn std::error::Error>> {
    let registry = [
        (FrameKind::Hello, 1_u8),
        (FrameKind::Welcome, 2),
        (FrameKind::Ping, 3),
        (FrameKind::Prepare, 4),
        (FrameKind::Execute, 5),
        (FrameKind::Begin, 6),
        (FrameKind::Commit, 7),
        (FrameKind::Rollback, 8),
        (FrameKind::Structure, 9),
        (FrameKind::Search, 10),
        (FrameKind::Value, 11),
        (FrameKind::Receipt, 12),
        (FrameKind::Failure, 13),
        (FrameKind::Cancel, 14),
        (FrameKind::Close, 15),
        (FrameKind::Deallocate, 16),
        (FrameKind::Explain, 17),
        (FrameKind::Savepoint, 18),
        (FrameKind::Data, 19),
        (FrameKind::End, 20),
        (FrameKind::WindowUpdate, 21),
        (FrameKind::RowSchema, 22),
        (FrameKind::RowBatch, 23),
        (FrameKind::Deadline, 24),
    ];

    for (kind, code) in registry {
        assert_eq!(kind as u8, code);
        assert_eq!(FrameKind::try_from(code)?, kind);
        let frame = encode_frame(kind, 7, 11, &[], 0)?;
        assert_eq!(decode_frame(&frame, 0)?.kind, kind);
    }
    assert_eq!(
        FrameKind::try_from(25),
        Err(LocalProtocolError::UnknownKind(25))
    );
    Ok(())
}
