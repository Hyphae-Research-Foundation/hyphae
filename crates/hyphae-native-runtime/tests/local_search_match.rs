// SPDX-License-Identifier: Apache-2.0

// Exercises the deprecated pre-daemon local session/transport on purpose.
#![allow(deprecated)]

//! Native local lexical MATCH codec and session coverage.

use hyphae_native_runtime::{
    LOCAL_SEARCH_MATCH_HEADER_SIZE, LOCAL_SEARCH_RESULTS_HEADER_SIZE, LocalSearchCodecError,
    LocalSearchMatchHit, MAX_LOCAL_SEARCH_HITS, MAX_LOCAL_SEARCH_QUERY_BYTES, MatchHit,
    decode_local_search_match, decode_local_search_match_results, encode_local_search_match,
    encode_local_search_match_results,
};
use hyphae_native_types::{Csn, ObjectId};

#[test]
fn local_search_match_codecs_have_stable_golden_bytes() -> Result<(), Box<dyn std::error::Error>> {
    let mut buffer = Vec::new();
    let index = ObjectId::new(2)?;
    assert_eq!(
        encode_local_search_match(&mut buffer, index, "rust", 3, 32)?,
        [
            1, 1, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 3, 0, 0, 0,
            b'r', b'u', b's', b't',
        ]
    );
    let request = decode_local_search_match(&buffer)?;
    assert_eq!(request.index, index);
    assert_eq!(request.query, "rust");
    assert_eq!(request.limit, 3);

    let hits = [
        MatchHit {
            document_id: b"a".to_vec(),
            score: 2.0,
        },
        MatchHit {
            document_id: b"\0b".to_vec(),
            score: 1.0,
        },
    ];
    assert_eq!(
        encode_local_search_match_results(&mut buffer, Csn::new(3)?, &hits, 43)?,
        [
            1, 1, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 64,
            b'a', 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 240, 63, 0, b'b',
        ]
    );
    let results = decode_local_search_match_results(&buffer)?;
    assert_eq!(results.visible_csn, Csn::new(3)?);
    assert_eq!(
        results.hits,
        [
            LocalSearchMatchHit {
                document_id: b"a",
                score: 2.0,
            },
            LocalSearchMatchHit {
                document_id: b"\0b",
                score: 1.0,
            },
        ]
    );
    Ok(())
}

#[test]
fn local_search_request_enforces_every_bound() -> Result<(), Box<dyn std::error::Error>> {
    let mut buffer = Vec::new();
    let index = ObjectId::new(1)?;
    let exact_query = "q".repeat(MAX_LOCAL_SEARCH_QUERY_BYTES);
    let encoded = encode_local_search_match(
        &mut buffer,
        index,
        &exact_query,
        MAX_LOCAL_SEARCH_HITS,
        LOCAL_SEARCH_MATCH_HEADER_SIZE + exact_query.len(),
    )?;
    assert_eq!(decode_local_search_match(encoded)?.query, exact_query);
    assert!(matches!(
        encode_local_search_match(
            &mut buffer,
            index,
            &"q".repeat(MAX_LOCAL_SEARCH_QUERY_BYTES + 1),
            1,
            usize::MAX,
        ),
        Err(LocalSearchCodecError::QueryTooLarge)
    ));
    for limit in [0, MAX_LOCAL_SEARCH_HITS + 1] {
        assert!(matches!(
            encode_local_search_match(&mut buffer, index, "", limit, usize::MAX),
            Err(LocalSearchCodecError::InvalidLimit(_))
        ));
    }
    assert!(matches!(
        encode_local_search_match(&mut buffer, index, "q", 1, LOCAL_SEARCH_MATCH_HEADER_SIZE,),
        Err(LocalSearchCodecError::PayloadTooLarge)
    ));

    let canonical = encode_local_search_match(&mut buffer, index, "", 1, usize::MAX)?.to_vec();
    for length in 0..LOCAL_SEARCH_MATCH_HEADER_SIZE {
        assert!(matches!(
            decode_local_search_match(&canonical[..length]),
            Err(LocalSearchCodecError::Truncated)
        ));
    }
    let with_query = encode_local_search_match(&mut buffer, index, "rust", 1, usize::MAX)?.to_vec();
    for length in LOCAL_SEARCH_MATCH_HEADER_SIZE..with_query.len() {
        assert!(matches!(
            decode_local_search_match(&with_query[..length]),
            Err(LocalSearchCodecError::LengthMismatch)
        ));
    }
    let mut invalid = canonical.clone();
    invalid[0] = 2;
    assert!(matches!(
        decode_local_search_match(&invalid),
        Err(LocalSearchCodecError::UnsupportedVersion(2))
    ));
    invalid = canonical.clone();
    invalid[1] = 2;
    assert!(matches!(
        decode_local_search_match(&invalid),
        Err(LocalSearchCodecError::UnknownOpcode(2))
    ));
    invalid = canonical.clone();
    invalid[2] = 1;
    assert!(matches!(
        decode_local_search_match(&invalid),
        Err(LocalSearchCodecError::ReservedBytes)
    ));
    invalid = canonical.clone();
    invalid[4..20].fill(0);
    assert!(matches!(
        decode_local_search_match(&invalid),
        Err(LocalSearchCodecError::InvalidObjectId)
    ));
    invalid = canonical.clone();
    invalid[24..28].fill(0);
    assert!(matches!(
        decode_local_search_match(&invalid),
        Err(LocalSearchCodecError::InvalidLimit(0))
    ));
    invalid = canonical.clone();
    invalid[20..24].copy_from_slice(&1_u32.to_le_bytes());
    invalid.push(0xff);
    assert!(matches!(
        decode_local_search_match(&invalid),
        Err(LocalSearchCodecError::InvalidUtf8)
    ));
    invalid.push(0);
    assert!(matches!(
        decode_local_search_match(&invalid),
        Err(LocalSearchCodecError::LengthMismatch)
    ));
    Ok(())
}

#[test]
fn local_search_results_reject_noncanonical_scores_order_and_lengths()
-> Result<(), Box<dyn std::error::Error>> {
    let mut buffer = Vec::new();
    let canonical = encode_local_search_match_results(
        &mut buffer,
        Csn::new(1)?,
        &[MatchHit {
            document_id: b"a".to_vec(),
            score: 1.0,
        }],
        usize::MAX,
    )?
    .to_vec();
    for length in 0..LOCAL_SEARCH_RESULTS_HEADER_SIZE {
        assert!(matches!(
            decode_local_search_match_results(&canonical[..length]),
            Err(LocalSearchCodecError::Truncated)
        ));
    }
    for length in LOCAL_SEARCH_RESULTS_HEADER_SIZE..canonical.len() {
        assert!(matches!(
            decode_local_search_match_results(&canonical[..length]),
            Err(LocalSearchCodecError::Truncated | LocalSearchCodecError::LengthMismatch)
        ));
    }
    let mut invalid = canonical.clone();
    invalid[0] = 2;
    assert!(matches!(
        decode_local_search_match_results(&invalid),
        Err(LocalSearchCodecError::UnsupportedVersion(2))
    ));
    invalid = canonical.clone();
    invalid[1] = 2;
    assert!(matches!(
        decode_local_search_match_results(&invalid),
        Err(LocalSearchCodecError::UnknownResultTag(2))
    ));
    invalid = canonical.clone();
    invalid[3] = 1;
    assert!(matches!(
        decode_local_search_match_results(&invalid),
        Err(LocalSearchCodecError::ReservedBytes)
    ));
    invalid = canonical.clone();
    invalid[8..16].fill(0);
    assert!(matches!(
        decode_local_search_match_results(&invalid),
        Err(LocalSearchCodecError::InvalidCsn)
    ));
    invalid = canonical.clone();
    invalid[4..8].copy_from_slice(&u32::try_from(MAX_LOCAL_SEARCH_HITS + 1)?.to_le_bytes());
    assert!(matches!(
        decode_local_search_match_results(&invalid),
        Err(LocalSearchCodecError::TooManyHits)
    ));
    for score in [0.0, -0.0, -1.0, f64::INFINITY, f64::NAN] {
        invalid = canonical.clone();
        invalid[20..28].copy_from_slice(&score.to_bits().to_le_bytes());
        assert!(matches!(
            decode_local_search_match_results(&invalid),
            Err(LocalSearchCodecError::NoncanonicalScore)
        ));
    }
    invalid = canonical.clone();
    invalid[16..20].copy_from_slice(&2_u32.to_le_bytes());
    assert!(matches!(
        decode_local_search_match_results(&invalid),
        Err(LocalSearchCodecError::LengthMismatch)
    ));
    invalid = canonical.clone();
    invalid.push(0);
    assert!(matches!(
        decode_local_search_match_results(&invalid),
        Err(LocalSearchCodecError::LengthMismatch)
    ));
    Ok(())
}

#[test]
fn local_search_results_reject_noncanonical_order() -> Result<(), Box<dyn std::error::Error>> {
    let mut buffer = Vec::new();
    let out_of_order = [
        MatchHit {
            document_id: b"b".to_vec(),
            score: 1.0,
        },
        MatchHit {
            document_id: b"a".to_vec(),
            score: 1.0,
        },
    ];
    assert!(matches!(
        encode_local_search_match_results(&mut buffer, Csn::new(1)?, &out_of_order, usize::MAX,),
        Err(LocalSearchCodecError::NoncanonicalHitOrder)
    ));
    let duplicate = [
        MatchHit {
            document_id: b"a".to_vec(),
            score: 1.0,
        },
        MatchHit {
            document_id: b"a".to_vec(),
            score: 1.0,
        },
    ];
    assert!(matches!(
        encode_local_search_match_results(&mut buffer, Csn::new(1)?, &duplicate, usize::MAX,),
        Err(LocalSearchCodecError::NoncanonicalHitOrder)
    ));

    let ordered = encode_local_search_match_results(
        &mut buffer,
        Csn::new(1)?,
        &[
            MatchHit {
                document_id: b"a".to_vec(),
                score: 1.0,
            },
            MatchHit {
                document_id: b"b".to_vec(),
                score: 1.0,
            },
        ],
        usize::MAX,
    )?
    .to_vec();
    let mut invalid = ordered.clone();
    invalid[28] = b'b';
    invalid[41] = b'a';
    assert!(matches!(
        decode_local_search_match_results(&invalid),
        Err(LocalSearchCodecError::NoncanonicalHitOrder)
    ));
    invalid = ordered;
    invalid[41] = b'a';
    assert!(matches!(
        decode_local_search_match_results(&invalid),
        Err(LocalSearchCodecError::NoncanonicalHitOrder)
    ));
    Ok(())
}

#[test]
fn local_search_result_enforces_hit_and_payload_bounds() -> Result<(), Box<dyn std::error::Error>> {
    let mut buffer = Vec::new();
    let hits = (0..MAX_LOCAL_SEARCH_HITS)
        .map(|index| {
            Ok(MatchHit {
                document_id: u32::try_from(index)?.to_be_bytes().to_vec(),
                score: 1.0,
            })
        })
        .collect::<Result<Vec<_>, std::num::TryFromIntError>>()?;
    let encoded = encode_local_search_match_results(&mut buffer, Csn::new(1)?, &hits, usize::MAX)?;
    assert_eq!(
        decode_local_search_match_results(encoded)?.hits.len(),
        MAX_LOCAL_SEARCH_HITS
    );
    let mut too_many = hits;
    too_many.push(MatchHit {
        document_id: b"overflow".to_vec(),
        score: 1.0,
    });
    assert!(matches!(
        encode_local_search_match_results(&mut buffer, Csn::new(1)?, &too_many, usize::MAX,),
        Err(LocalSearchCodecError::TooManyHits)
    ));
    assert!(matches!(
        encode_local_search_match_results(
            &mut buffer,
            Csn::new(1)?,
            &[MatchHit {
                document_id: b"a".to_vec(),
                score: 1.0,
            }],
            LOCAL_SEARCH_RESULTS_HEADER_SIZE,
        ),
        Err(LocalSearchCodecError::PayloadTooLarge)
    ));
    Ok(())
}

#[cfg(unix)]
mod unix {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };

    use hyphae_native_runtime::{
        FrameKind, LocalDataSession, LocalFailureCode, NativeDatabase, NativeSchedulerClock,
        UdsFrameConnection, UdsFrameListener, decode_local_failure,
        decode_local_search_match_results, encode_local_search_match,
    };
    use hyphae_native_types::{DurabilityClass, ObjectId};

    const MAXIMUM_PAYLOAD: usize = 256;

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn create() -> Result<Self, Box<dyn std::error::Error>> {
            let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let path =
                Path::new("/tmp").join(format!("hy-match-{}-{timestamp}", std::process::id()));
            fs::create_dir(&path)?;
            Ok(Self(path))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ignored = fs::remove_dir_all(&self.0);
        }
    }

    struct CountingClock(AtomicUsize);

    impl NativeSchedulerClock for CountingClock {
        fn logical_time_micros(&self) -> i64 {
            self.0.fetch_add(1, Ordering::Relaxed);
            100
        }
    }

    fn receive(
        connection: &mut UdsFrameConnection,
        kind: FrameKind,
        stream_id: u32,
        request_id: u64,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let frame = connection.receive()?.ok_or("server closed early")?;
        if frame.kind != kind || frame.stream_id != stream_id || frame.request_id != request_id {
            return Err("response identity diverged".into());
        }
        Ok(frame.payload.to_vec())
    }

    fn send_match(
        connection: &mut UdsFrameConnection,
        buffer: &mut Vec<u8>,
        index: ObjectId,
        query: &str,
        limit: usize,
        request_id: u64,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let request = encode_local_search_match(buffer, index, query, limit, MAXIMUM_PAYLOAD)?;
        connection.send(FrameKind::Search, 7, request_id, request)?;
        receive(connection, FrameKind::Value, 7, request_id)
    }

    #[test]
    fn uds_session_matches_physical_search_recovers_failures_and_reopens()
    -> Result<(), Box<dyn std::error::Error>> {
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join("data");
        let socket = temporary.path().join("hyphae.sock");
        let mut database = NativeDatabase::create(&data)?;
        let index = ObjectId::new(1)?;
        let mut seed = database.begin(10, DurabilityClass::Memory)?;
        seed.create_search_index(index, "documents")?;
        seed.index_document(index, b"doc-a".to_vec(), "rust search common")?;
        seed.index_document(index, b"doc-b".to_vec(), "rust rust common")?;
        seed.index_document(index, b"doc-c".to_vec(), "sql common")?;
        seed.index_document(index, b"doc-d".to_vec(), "tie")?;
        seed.index_document(index, b"doc-e".to_vec(), "tie")?;
        seed.index_document(index, vec![b'x'; 230], "oversizedresponse")?;
        seed.commit()?;
        let expected_rust = database.match_latest_text(index, "rust", 10)?;
        let expected_common = database.match_latest_text(index, "common", 10)?;
        let expected_tie = database.match_latest_text(index, "tie", 10)?;

        let clock = Arc::new(CountingClock(AtomicUsize::new(0)));
        let server_clock = Arc::clone(&clock);
        let listener = UdsFrameListener::bind(&socket, MAXIMUM_PAYLOAD)?;
        let server = thread::spawn(move || {
            let mut connection = listener.accept()?;
            LocalDataSession::new(&mut database, server_clock.as_ref()).serve(&mut connection)?;
            listener.close()?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        });

        let mut client = UdsFrameConnection::connect(&socket, MAXIMUM_PAYLOAD)?;
        client.send(FrameKind::Hello, 0, 1, b"")?;
        assert_eq!(receive(&mut client, FrameKind::Welcome, 0, 1)?, b"");
        let mut buffer = Vec::new();

        client.send(FrameKind::Search, 7, 2, &[1])?;
        assert_eq!(
            decode_local_failure(&receive(&mut client, FrameKind::Failure, 7, 2)?)?,
            LocalFailureCode::InvalidRequest
        );

        let unknown =
            encode_local_search_match(&mut buffer, ObjectId::new(9)?, "rust", 10, MAXIMUM_PAYLOAD)?;
        client.send(FrameKind::Search, 7, 3, unknown)?;
        assert_eq!(
            decode_local_failure(&receive(&mut client, FrameKind::Failure, 7, 3)?)?,
            LocalFailureCode::EngineFailure
        );

        let oversized =
            encode_local_search_match(&mut buffer, index, "oversizedresponse", 1, MAXIMUM_PAYLOAD)?;
        client.send(FrameKind::Search, 7, 4, oversized)?;
        assert_eq!(
            decode_local_failure(&receive(&mut client, FrameKind::Failure, 7, 4)?)?,
            LocalFailureCode::ResponseTooLarge
        );

        let empty = send_match(&mut client, &mut buffer, index, "", 10, 5)?;
        let empty = decode_local_search_match_results(&empty)?;
        assert_eq!(empty.visible_csn.get(), 1);
        assert!(empty.hits.is_empty());

        let missing = send_match(&mut client, &mut buffer, index, "missing", 10, 6)?;
        assert!(decode_local_search_match_results(&missing)?.hits.is_empty());

        for (query, expected, request_id) in [
            ("rust", expected_rust.as_slice(), 7),
            ("common", expected_common.as_slice(), 8),
            ("tie", expected_tie.as_slice(), 9),
        ] {
            let response = send_match(&mut client, &mut buffer, index, query, 10, request_id)?;
            let response = decode_local_search_match_results(&response)?;
            assert_eq!(response.visible_csn.get(), 1);
            assert_eq!(response.hits.len(), expected.len());
            for (actual, expected) in response.hits.iter().zip(expected) {
                assert_eq!(actual.document_id, expected.document_id);
                assert_eq!(actual.score.to_bits(), expected.score.to_bits());
            }
        }
        assert_eq!(
            expected_tie
                .iter()
                .map(|hit| hit.document_id.as_slice())
                .collect::<Vec<_>>(),
            [b"doc-d".as_slice(), b"doc-e".as_slice()]
        );

        client.send(FrameKind::Close, 0, 10, b"")?;
        assert_eq!(receive(&mut client, FrameKind::Close, 0, 10)?, b"");
        server
            .join()
            .map_err(|_| std::io::Error::other("local MATCH server panicked"))?
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        assert_eq!(clock.0.load(Ordering::Relaxed), 0);

        let reopened = NativeDatabase::open(&data)?;
        assert_eq!(
            reopened.match_latest_text(index, "rust", 10)?,
            expected_rust
        );
        assert_eq!(
            reopened.match_latest_text(index, "common", 10)?,
            expected_common
        );
        assert_eq!(reopened.match_latest_text(index, "tie", 10)?, expected_tie);
        Ok(())
    }
}
