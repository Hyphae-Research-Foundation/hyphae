// SPDX-License-Identifier: Apache-2.0

// Exercises the deprecated pre-daemon local session/transport on purpose.
#![allow(deprecated)]

//! Direct-Linux native lexical MATCH transport latency observation.

#[cfg(not(unix))]
fn main() {
    eprintln!("uds_search_match_smoke is available only on Unix targets");
}

#[cfg(unix)]
mod unix {
    use std::{
        fs,
        hint::black_box,
        path::{Path, PathBuf},
        thread,
        time::{Instant, SystemTime, UNIX_EPOCH},
    };

    use hyphae_native_runtime::{
        FrameKind, LocalDataSession, MatchHit, NativeDatabase, NativeSchedulerClock,
        UdsFrameConnection, UdsFrameListener, decode_local_search_match_results,
        encode_local_search_match,
    };
    use hyphae_native_types::{Csn, DurabilityClass, ObjectId};

    const MAXIMUM_PAYLOAD: usize = 128;
    const DOCUMENT_COUNT: u32 = 2_048;
    const TARGET_DOCUMENT: u32 = 1_024;
    const QUERY: &str = "needle";
    const RESULT_LIMIT: usize = 10;
    const RESULT_BYTES: usize = 32;
    const PING_BYTES: usize = 32;
    const OBSERVATIONS: usize = 100_000;
    const WARMUP: usize = 10_000;
    const PING_STREAM_ID: u32 = 7;
    const SEARCH_STREAM_ID: u32 = 8;
    const HELLO_REQUEST_ID: u64 = 1;

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn create() -> Result<Self, Box<dyn std::error::Error>> {
            let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let path = std::env::temp_dir().join(format!(
                "hyphae-local-search-match-smoke-{}-{timestamp}",
                std::process::id()
            ));
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

    struct FixedClock;

    impl NativeSchedulerClock for FixedClock {
        fn logical_time_micros(&self) -> i64 {
            0
        }
    }

    #[derive(Clone, Copy)]
    struct Stats {
        p50_nanos: u64,
        p95_nanos: u64,
        p99_nanos: u64,
        p999_nanos: u64,
        maximum_nanos: u64,
        throughput_per_second: f64,
    }

    fn percentile(samples: &[u64], per_mille: usize) -> u64 {
        let index = samples.len().saturating_sub(1).saturating_mul(per_mille) / 1_000;
        samples[index]
    }

    fn measure<E>(
        mut operation: impl FnMut() -> Result<(), E>,
    ) -> Result<Stats, Box<dyn std::error::Error>>
    where
        E: std::error::Error + 'static,
    {
        let mut samples = Vec::with_capacity(OBSERVATIONS);
        let total = Instant::now();
        for _ in 0..OBSERVATIONS {
            let started = Instant::now();
            operation()?;
            samples.push(u64::try_from(started.elapsed().as_nanos())?);
        }
        let elapsed = total.elapsed();
        samples.sort_unstable();
        let completed = u32::try_from(samples.len())?;
        Ok(Stats {
            p50_nanos: percentile(&samples, 500),
            p95_nanos: percentile(&samples, 950),
            p99_nanos: percentile(&samples, 990),
            p999_nanos: percentile(&samples, 999),
            maximum_nanos: *samples.last().ok_or("MATCH smoke produced no samples")?,
            throughput_per_second: f64::from(completed) / elapsed.as_secs_f64(),
        })
    }

    fn document_text(sequence: u32) -> &'static str {
        if sequence == TARGET_DOCUMENT {
            "needle common"
        } else if sequence.is_multiple_of(2) {
            "rust common"
        } else {
            "sql common"
        }
    }

    struct PreparedCorpus {
        database: NativeDatabase,
        index: ObjectId,
        expected: MatchHit,
        dataset_digest: String,
        tree_height: usize,
    }

    fn prepare_database(directory: &Path) -> Result<PreparedCorpus, Box<dyn std::error::Error>> {
        let mut database = NativeDatabase::create(directory)?;
        let index = ObjectId::new(1)?;
        let mut transaction = database.begin(100, DurabilityClass::Memory)?;
        transaction.create_search_index(index, "documents")?;
        let mut dataset = blake3::Hasher::new();
        for sequence in 0..DOCUMENT_COUNT {
            let document_id = sequence.to_be_bytes();
            let text = document_text(sequence);
            dataset.update(&document_id);
            dataset.update(&u32::try_from(text.len())?.to_le_bytes());
            dataset.update(text.as_bytes());
            transaction.index_document(index, document_id.to_vec(), text)?;
        }
        let receipt = transaction.commit()?;
        if receipt.commit_csn != Csn::new(1)? {
            return Err("MATCH smoke seed CSN diverged".into());
        }
        let tree_height = database.latest_search_tree_height()?;
        if tree_height < 2 {
            return Err("MATCH smoke did not build a multilevel B+tree".into());
        }
        let mut expected = database.match_latest_text(index, QUERY, RESULT_LIMIT)?;
        if expected.len() != 1 || expected[0].document_id != TARGET_DOCUMENT.to_be_bytes() {
            return Err("MATCH smoke rare-term result diverged".into());
        }
        Ok(PreparedCorpus {
            database,
            index,
            expected: expected.remove(0),
            dataset_digest: dataset.finalize().to_hex().to_string(),
            tree_height,
        })
    }

    fn require_ping(
        connection: &mut UdsFrameConnection,
        request_id: u64,
        payload: &[u8],
    ) -> Result<(), Box<dyn std::error::Error>> {
        connection.send(FrameKind::Ping, PING_STREAM_ID, request_id, payload)?;
        let frame = connection.receive()?.ok_or("server closed before PING")?;
        if frame.kind != FrameKind::Ping
            || frame.stream_id != PING_STREAM_ID
            || frame.request_id != request_id
            || frame.payload != payload
        {
            return Err("PING response diverged".into());
        }
        Ok(())
    }

    fn require_match(
        connection: &mut UdsFrameConnection,
        request_id: u64,
        request: &[u8],
        expected: &MatchHit,
    ) -> Result<(), Box<dyn std::error::Error>> {
        connection.send(FrameKind::Search, SEARCH_STREAM_ID, request_id, request)?;
        let frame = connection
            .receive()?
            .ok_or("server closed before SEARCH MATCH")?;
        if frame.kind != FrameKind::Value
            || frame.stream_id != SEARCH_STREAM_ID
            || frame.request_id != request_id
            || frame.payload.len() != RESULT_BYTES
        {
            return Err("SEARCH MATCH response identity diverged".into());
        }
        let results = decode_local_search_match_results(frame.payload)?;
        if results.visible_csn != Csn::new(1)?
            || results.hits.len() != 1
            || results.hits[0].document_id != expected.document_id
            || results.hits[0].score.to_bits() != expected.score.to_bits()
        {
            return Err("SEARCH MATCH result diverged".into());
        }
        Ok(())
    }

    fn measure_embedded(
        database: &NativeDatabase,
        index: ObjectId,
        expected: &MatchHit,
    ) -> Result<Stats, Box<dyn std::error::Error>> {
        for _ in 0..WARMUP {
            let observed = database.match_latest_text(index, black_box(QUERY), RESULT_LIMIT)?;
            if !matches_expected(&observed, expected) {
                return Err("embedded MATCH warmup diverged".into());
            }
        }
        measure(|| {
            let observed = database
                .match_latest_text(index, black_box(QUERY), RESULT_LIMIT)
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            if !matches_expected(black_box(&observed), expected) {
                return Err(std::io::Error::other("embedded MATCH diverged"));
            }
            Ok(())
        })
    }

    fn matches_expected(observed: &[MatchHit], expected: &MatchHit) -> bool {
        observed.len() == 1
            && observed[0].document_id == expected.document_id
            && observed[0].score.to_bits() == expected.score.to_bits()
    }

    type ServerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
    type ServerHandle = thread::JoinHandle<ServerResult>;

    fn start_server(
        mut database: NativeDatabase,
        socket: &Path,
    ) -> Result<(UdsFrameConnection, ServerHandle), Box<dyn std::error::Error>> {
        let listener = UdsFrameListener::bind(socket, MAXIMUM_PAYLOAD)?;
        let server = thread::spawn(move || {
            let mut connection = listener.accept()?;
            let clock = FixedClock;
            LocalDataSession::new(&mut database, &clock).serve(&mut connection)?;
            listener.close()?;
            Ok(())
        });
        let connection = UdsFrameConnection::connect(socket, MAXIMUM_PAYLOAD)?;
        Ok((connection, server))
    }

    fn handshake(connection: &mut UdsFrameConnection) -> Result<(), Box<dyn std::error::Error>> {
        connection.send(FrameKind::Hello, 0, HELLO_REQUEST_ID, b"")?;
        let welcome = connection
            .receive()?
            .ok_or("server closed before WELCOME")?;
        if welcome.kind != FrameKind::Welcome
            || welcome.stream_id != 0
            || welcome.request_id != HELLO_REQUEST_ID
            || !welcome.payload.is_empty()
        {
            return Err("WELCOME response diverged".into());
        }
        Ok(())
    }

    fn measure_remote(
        mut connection: UdsFrameConnection,
        server: ServerHandle,
        index: ObjectId,
        expected: &MatchHit,
    ) -> Result<(Stats, Stats), Box<dyn std::error::Error>> {
        handshake(&mut connection)?;
        let ping_payload = [0xa5; PING_BYTES];
        let mut request_buffer = Vec::new();
        let match_request = encode_local_search_match(
            &mut request_buffer,
            index,
            QUERY,
            RESULT_LIMIT,
            MAXIMUM_PAYLOAD,
        )?
        .to_vec();
        let mut next_request_id = HELLO_REQUEST_ID + 1;
        for _ in 0..WARMUP {
            require_ping(&mut connection, next_request_id, &ping_payload)?;
            next_request_id += 1;
        }
        for _ in 0..WARMUP {
            require_match(&mut connection, next_request_id, &match_request, expected)?;
            next_request_id += 1;
        }

        let ping = measure(|| {
            let request_id = next_request_id;
            next_request_id = next_request_id
                .checked_add(1)
                .ok_or_else(|| std::io::Error::other("PING request ID overflow"))?;
            require_ping(&mut connection, request_id, &ping_payload)
                .map_err(|error| std::io::Error::other(error.to_string()))
        })?;
        let matched = measure(|| {
            let request_id = next_request_id;
            next_request_id = next_request_id
                .checked_add(1)
                .ok_or_else(|| std::io::Error::other("MATCH request ID overflow"))?;
            require_match(&mut connection, request_id, &match_request, expected)
                .map_err(|error| std::io::Error::other(error.to_string()))
        })?;

        connection.send(FrameKind::Close, 0, next_request_id, b"")?;
        let close = connection.receive()?.ok_or("server closed before CLOSE")?;
        if close.kind != FrameKind::Close
            || close.stream_id != 0
            || close.request_id != next_request_id
            || !close.payload.is_empty()
        {
            return Err("CLOSE response diverged".into());
        }
        server
            .join()
            .map_err(|_| std::io::Error::other("SEARCH MATCH server panicked"))?
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        Ok((ping, matched))
    }

    fn print_stats(name: &str, stats: Stats, trailing_comma: bool) {
        println!("    \"{name}\": {{");
        println!("      \"p50_nanos\": {},", stats.p50_nanos);
        println!("      \"p95_nanos\": {},", stats.p95_nanos);
        println!("      \"p99_nanos\": {},", stats.p99_nanos);
        println!("      \"p999_nanos\": {},", stats.p999_nanos);
        println!("      \"maximum_nanos\": {},", stats.maximum_nanos);
        println!(
            "      \"throughput_per_second\": {:.3}",
            stats.throughput_per_second
        );
        println!("    }}{}", if trailing_comma { "," } else { "" });
    }

    struct Receipt<'value> {
        commit: &'value str,
        harness_commit: &'value str,
        dataset_digest: &'value str,
        tree_height: usize,
        embedded: Stats,
        ping: Stats,
        matched: Stats,
    }

    fn print_receipt(receipt: &Receipt<'_>) {
        println!("{{");
        println!("  \"schema\": \"hyphae.native.local-search-match-smoke.v1\",");
        println!("  \"status\": \"observation-not-regression-gate\",");
        println!("  \"commit\": \"{}\",", receipt.commit);
        println!("  \"harness_commit\": \"{}\",", receipt.harness_commit);
        println!("  \"target\": \"x86_64-linux\",");
        println!("  \"profile\": \"release\",");
        println!("  \"concurrency\": 1,");
        println!("  \"warm_state\": true,");
        println!("  \"durability\": \"memory\",");
        println!("  \"maximum_payload_bytes\": {MAXIMUM_PAYLOAD},");
        println!("  \"search_documents\": {DOCUMENT_COUNT},");
        println!("  \"search_tree_height\": {},", receipt.tree_height);
        println!("  \"query_bytes\": {},", QUERY.len());
        println!("  \"result_limit\": {RESULT_LIMIT},");
        println!("  \"result_count\": 1,");
        println!("  \"result_bytes\": {RESULT_BYTES},");
        println!("  \"ping_bytes\": {PING_BYTES},");
        println!("  \"warmup_per_operation\": {WARMUP},");
        println!("  \"observations_per_operation\": {OBSERVATIONS},");
        println!(
            "  \"dataset_digest_blake3\": \"{}\",",
            receipt.dataset_digest
        );
        println!("  \"operations\": {{");
        print_stats(
            "embedded_physical_search_match_one_hit",
            receipt.embedded,
            true,
        );
        print_stats("persistent_ping_round_trip_32b", receipt.ping, true);
        print_stats(
            "persistent_search_match_round_trip_one_hit",
            receipt.matched,
            false,
        );
        println!("  }}");
        println!("}}");
    }

    pub(super) fn run() -> Result<(), Box<dyn std::error::Error>> {
        let commit = std::env::args()
            .nth(1)
            .unwrap_or_else(|| "unknown".to_owned());
        let harness_commit = std::env::args()
            .nth(2)
            .unwrap_or_else(|| "unknown".to_owned());
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join("data");
        let socket = temporary.path().join("hyphae.sock");
        let corpus = prepare_database(&data)?;
        let embedded = measure_embedded(&corpus.database, corpus.index, &corpus.expected)?;
        let (connection, server) = start_server(corpus.database, &socket)?;
        let (ping, matched) = measure_remote(connection, server, corpus.index, &corpus.expected)?;
        print_receipt(&Receipt {
            commit: &commit,
            harness_commit: &harness_commit,
            dataset_digest: &corpus.dataset_digest,
            tree_height: corpus.tree_height,
            embedded,
            ping,
            matched,
        });
        Ok(())
    }
}

#[cfg(unix)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    unix::run()
}
