// SPDX-License-Identifier: Apache-2.0

// Exercises the deprecated pre-daemon local session/transport on purpose.
#![allow(deprecated)]

//! Direct-Linux all-engine local transaction latency observations.

#[cfg(not(unix))]
fn main() {
    eprintln!("uds_all_engine_transaction_smoke is available only on Unix targets");
}

#[cfg(unix)]
mod unix {
    use std::{
        fs,
        num::NonZeroU64,
        path::{Path, PathBuf},
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use hyphae_native_runtime::{
        FrameKind, LocalDataSession, LocalTransactionBeginReceipt, LocalTransactionCommitReceipt,
        LocalTransactionEngine, LocalTransactionIndexDocumentRequest,
        LocalTransactionRollbackReceipt, LocalTransactionStageReceipt,
        LocalTransactionStructureSetRequest, NativeDatabase, NativeSchedulerClock,
        UdsFrameConnection, UdsFrameListener, decode_local_transaction_begin_receipt,
        decode_local_transaction_commit_receipt, decode_local_transaction_rollback_receipt,
        decode_local_transaction_stage_receipt, encode_local_transaction_begin,
        encode_local_transaction_commit, encode_local_transaction_index_document,
        encode_local_transaction_rollback, encode_local_transaction_sql_dml,
        encode_local_transaction_structure_set,
    };
    use hyphae_native_types::{Csn, DurabilityClass, ObjectId, ScalarValue};

    const MAXIMUM_PAYLOAD: usize = 512;
    const INDEX_ID: u128 = 100;
    const STREAM_ID: u32 = 19;
    const LOGICAL_TIME_MICROS: i64 = 100;
    const PING_BYTES: usize = 32;
    const PING_WARMUP: usize = 10_000;
    const PING_OBSERVATIONS: usize = 100_000;
    const STAGE_WARMUP: usize = 1_000;
    const STAGE_OBSERVATIONS: usize = 10_000;
    const MEMORY_COMMIT_WARMUP: usize = 16;
    const MEMORY_COMMIT_OBSERVATIONS: usize = 256;
    const STRICT_COMMIT_WARMUP: usize = 16;
    const STRICT_COMMIT_OBSERVATIONS: usize = 256;
    const INSERT: &str = "INSERT INTO benchmark_events (id, body) VALUES (?, ?)";
    const UPDATE: &str = "UPDATE benchmark_events SET body = ? WHERE id = ?";
    const COMMIT_KEY: &[u8] = b"benchmark-commit-key";

    type TestError = Box<dyn std::error::Error>;
    type ServerError = Box<dyn std::error::Error + Send + Sync>;
    type ServerHandle = thread::JoinHandle<Result<(), ServerError>>;

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn create() -> Result<Self, TestError> {
            let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let path = std::env::temp_dir().join(format!(
                "hyphae-all-engine-transaction-smoke-{}-{timestamp}",
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
            LOGICAL_TIME_MICROS
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

    impl Stats {
        fn from_samples(mut samples: Vec<u64>, elapsed: Duration) -> Result<Self, TestError> {
            samples.sort_unstable();
            let completed = u32::try_from(samples.len())?;
            Ok(Self {
                p50_nanos: percentile(&samples, 500),
                p95_nanos: percentile(&samples, 950),
                p99_nanos: percentile(&samples, 990),
                p999_nanos: percentile(&samples, 999),
                maximum_nanos: *samples.last().ok_or("benchmark produced no samples")?,
                throughput_per_second: f64::from(completed) / elapsed.as_secs_f64(),
            })
        }
    }

    fn percentile(samples: &[u64], per_mille: usize) -> u64 {
        let index = samples.len().saturating_sub(1).saturating_mul(per_mille) / 1_000;
        samples[index]
    }

    fn elapsed_nanos(started: Instant) -> Result<(u64, Duration), TestError> {
        let elapsed = started.elapsed();
        Ok((u64::try_from(elapsed.as_nanos())?, elapsed))
    }

    fn seed_database(directory: &Path) -> Result<NativeDatabase, TestError> {
        let mut database = NativeDatabase::create(directory)?;
        let mut transaction = database.begin(LOGICAL_TIME_MICROS, DurabilityClass::Strict)?;
        transaction.execute_sql(
            "CREATE TABLE benchmark_events (
                id BIGINT PRIMARY KEY,
                body TEXT NOT NULL
            )",
            &[],
        )?;
        transaction.create_search_index(ObjectId::new(INDEX_ID)?, "benchmark_documents")?;
        transaction.execute_sql(
            "INSERT INTO benchmark_events (id, body) VALUES (0, 'seed')",
            &[],
        )?;
        transaction.set(COMMIT_KEY.to_vec(), transaction_value(0).to_vec(), None)?;
        let receipt = transaction.commit()?;
        if receipt.commit_csn != Csn::new(1)? {
            return Err("benchmark seed CSN diverged".into());
        }
        Ok(database)
    }

    fn start_server(
        database: NativeDatabase,
        socket: &Path,
    ) -> Result<(TransactionClient, ServerHandle), TestError> {
        let listener = UdsFrameListener::bind(socket, MAXIMUM_PAYLOAD)?;
        let server = thread::spawn(move || {
            let mut database = database;
            let mut connection = listener.accept()?;
            LocalDataSession::new(&mut database, &FixedClock).serve(&mut connection)?;
            listener.close()?;
            Ok(())
        });
        let client = TransactionClient::connect(socket)?;
        Ok((client, server))
    }

    fn join_server(server: ServerHandle) -> Result<(), TestError> {
        server
            .join()
            .map_err(|_| std::io::Error::other("transaction benchmark server panicked"))?
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        Ok(())
    }

    struct TransactionClient {
        connection: UdsFrameConnection,
        buffer: Vec<u8>,
        next_request_id: u64,
    }

    impl TransactionClient {
        fn connect(socket: &Path) -> Result<Self, TestError> {
            let mut connection = UdsFrameConnection::connect(socket, MAXIMUM_PAYLOAD)?;
            connection.send(FrameKind::Hello, 0, 1, b"")?;
            let welcome = connection
                .receive()?
                .ok_or("server closed before WELCOME")?;
            if welcome.kind != FrameKind::Welcome
                || welcome.stream_id != 0
                || welcome.request_id != 1
                || !welcome.payload.is_empty()
            {
                return Err("benchmark handshake diverged".into());
            }
            Ok(Self {
                connection,
                buffer: Vec::new(),
                next_request_id: 2,
            })
        }

        fn exchange(
            &mut self,
            request_kind: FrameKind,
            response_kind: FrameKind,
            payload: &[u8],
        ) -> Result<Vec<u8>, TestError> {
            let request_id = self.next_request_id;
            self.next_request_id = request_id.checked_add(1).ok_or("request ID overflow")?;
            self.connection
                .send(request_kind, STREAM_ID, request_id, payload)?;
            let response = self.connection.receive()?.ok_or("server closed early")?;
            if response.kind != response_kind
                || response.stream_id != STREAM_ID
                || response.request_id != request_id
            {
                return Err(format!(
                    "benchmark response diverged: expected {response_kind:?}/{STREAM_ID}/{request_id}, found {:?}/{}/{}",
                    response.kind, response.stream_id, response.request_id
                )
                .into());
            }
            Ok(response.payload.to_vec())
        }

        fn begin(
            &mut self,
            durability: DurabilityClass,
        ) -> Result<LocalTransactionBeginReceipt, TestError> {
            let request = encode_local_transaction_begin(&mut self.buffer, durability)?.to_vec();
            let response = self.exchange(FrameKind::Begin, FrameKind::Receipt, &request)?;
            Ok(decode_local_transaction_begin_receipt(&response)?)
        }

        fn stage_sql(
            &mut self,
            handle: NonZeroU64,
            sequence: u64,
        ) -> Result<LocalTransactionStageReceipt, TestError> {
            let sequence = i64::try_from(sequence)?;
            let request = encode_local_transaction_sql_dml(
                &mut self.buffer,
                handle,
                INSERT,
                &[
                    ScalarValue::Signed(sequence),
                    ScalarValue::Text(format!("benchmark transaction {sequence}")),
                ],
                MAXIMUM_PAYLOAD,
            )?
            .to_vec();
            let response = self.exchange(FrameKind::Execute, FrameKind::Receipt, &request)?;
            Ok(decode_local_transaction_stage_receipt(&response)?)
        }

        fn stage_structure(
            &mut self,
            handle: NonZeroU64,
            sequence: u64,
        ) -> Result<LocalTransactionStageReceipt, TestError> {
            let identity = sequence.to_be_bytes();
            let value = transaction_value(sequence);
            let request = encode_local_transaction_structure_set(
                &mut self.buffer,
                LocalTransactionStructureSetRequest {
                    handle,
                    key: &identity,
                    value: &value,
                    relative_ttl_micros: None,
                },
                MAXIMUM_PAYLOAD,
            )?
            .to_vec();
            let response = self.exchange(FrameKind::Structure, FrameKind::Receipt, &request)?;
            Ok(decode_local_transaction_stage_receipt(&response)?)
        }

        fn stage_update_sql(
            &mut self,
            handle: NonZeroU64,
            sequence: u64,
        ) -> Result<LocalTransactionStageReceipt, TestError> {
            let request = encode_local_transaction_sql_dml(
                &mut self.buffer,
                handle,
                UPDATE,
                &[
                    ScalarValue::Text(format!("benchmark transaction {sequence}")),
                    ScalarValue::Signed(0),
                ],
                MAXIMUM_PAYLOAD,
            )?
            .to_vec();
            let response = self.exchange(FrameKind::Execute, FrameKind::Receipt, &request)?;
            Ok(decode_local_transaction_stage_receipt(&response)?)
        }

        fn stage_commit_structure(
            &mut self,
            handle: NonZeroU64,
            sequence: u64,
        ) -> Result<LocalTransactionStageReceipt, TestError> {
            let value = transaction_value(sequence);
            let request = encode_local_transaction_structure_set(
                &mut self.buffer,
                LocalTransactionStructureSetRequest {
                    handle,
                    key: COMMIT_KEY,
                    value: &value,
                    relative_ttl_micros: None,
                },
                MAXIMUM_PAYLOAD,
            )?
            .to_vec();
            let response = self.exchange(FrameKind::Structure, FrameKind::Receipt, &request)?;
            Ok(decode_local_transaction_stage_receipt(&response)?)
        }

        fn stage_search(
            &mut self,
            handle: NonZeroU64,
            sequence: u64,
        ) -> Result<LocalTransactionStageReceipt, TestError> {
            let identity = sequence.to_be_bytes();
            let text = format!("benchmark transaction {sequence}");
            let request = encode_local_transaction_index_document(
                &mut self.buffer,
                LocalTransactionIndexDocumentRequest {
                    handle,
                    index: ObjectId::new(INDEX_ID)?,
                    document_id: &identity,
                    text: &text,
                },
                MAXIMUM_PAYLOAD,
            )?
            .to_vec();
            let response = self.exchange(FrameKind::Search, FrameKind::Receipt, &request)?;
            Ok(decode_local_transaction_stage_receipt(&response)?)
        }

        fn commit(
            &mut self,
            handle: NonZeroU64,
        ) -> Result<LocalTransactionCommitReceipt, TestError> {
            let request = encode_local_transaction_commit(&mut self.buffer, handle, 3)?.to_vec();
            let response = self.exchange(FrameKind::Commit, FrameKind::Receipt, &request)?;
            Ok(decode_local_transaction_commit_receipt(&response)?)
        }

        fn rollback(
            &mut self,
            handle: NonZeroU64,
        ) -> Result<LocalTransactionRollbackReceipt, TestError> {
            let request = encode_local_transaction_rollback(&mut self.buffer, handle).to_vec();
            let response = self.exchange(FrameKind::Rollback, FrameKind::Receipt, &request)?;
            Ok(decode_local_transaction_rollback_receipt(&response)?)
        }

        fn ping(&mut self, payload: &[u8]) -> Result<(), TestError> {
            let response = self.exchange(FrameKind::Ping, FrameKind::Ping, payload)?;
            if response != payload {
                return Err("benchmark PING response diverged".into());
            }
            Ok(())
        }

        fn close(mut self) -> Result<(), TestError> {
            let response = self.exchange(FrameKind::Close, FrameKind::Close, b"")?;
            if !response.is_empty() {
                return Err("benchmark CLOSE response diverged".into());
            }
            Ok(())
        }
    }

    fn transaction_value(sequence: u64) -> [u8; 32] {
        let identity = sequence.to_le_bytes();
        let mut value = [0_u8; 32];
        for (offset, byte) in value.iter_mut().enumerate() {
            *byte = identity[offset % identity.len()];
        }
        value
    }

    fn require_stage_receipt(
        receipt: LocalTransactionStageReceipt,
        handle: NonZeroU64,
        engine: LocalTransactionEngine,
        ordinal: u64,
    ) -> Result<(), TestError> {
        if receipt.engine != engine
            || receipt.handle != handle
            || receipt.operation_ordinal != ordinal
            || receipt.rows_affected != 1
        {
            return Err("benchmark stage receipt diverged".into());
        }
        Ok(())
    }

    fn prepare_transaction(
        client: &mut TransactionClient,
        durability: DurabilityClass,
        sequence: u64,
    ) -> Result<LocalTransactionBeginReceipt, TestError> {
        let begun = client.begin(durability)?;
        require_stage_receipt(
            client.stage_sql(begun.handle, sequence)?,
            begun.handle,
            LocalTransactionEngine::Relational,
            1,
        )?;
        require_stage_receipt(
            client.stage_structure(begun.handle, sequence)?,
            begun.handle,
            LocalTransactionEngine::Structure,
            2,
        )?;
        require_stage_receipt(
            client.stage_search(begun.handle, sequence)?,
            begun.handle,
            LocalTransactionEngine::Search,
            3,
        )?;
        Ok(begun)
    }

    fn prepare_commit_transaction(
        client: &mut TransactionClient,
        durability: DurabilityClass,
        sequence: u64,
    ) -> Result<LocalTransactionBeginReceipt, TestError> {
        let begun = client.begin(durability)?;
        require_stage_receipt(
            client.stage_update_sql(begun.handle, sequence)?,
            begun.handle,
            LocalTransactionEngine::Relational,
            1,
        )?;
        require_stage_receipt(
            client.stage_commit_structure(begun.handle, sequence)?,
            begun.handle,
            LocalTransactionEngine::Structure,
            2,
        )?;
        require_stage_receipt(
            client.stage_search(begun.handle, sequence)?,
            begun.handle,
            LocalTransactionEngine::Search,
            3,
        )?;
        Ok(begun)
    }

    fn stage_once(client: &mut TransactionClient, sequence: u64) -> Result<(), TestError> {
        let begun = prepare_transaction(client, DurabilityClass::Memory, sequence)?;
        let rolled_back = client.rollback(begun.handle)?;
        if rolled_back.handle != begun.handle || rolled_back.discarded_operations != 3 {
            return Err("benchmark rollback receipt diverged".into());
        }
        Ok(())
    }

    struct StageStats {
        sql: Stats,
        structure: Stats,
        search: Stats,
    }

    struct StageSamples {
        sql: Vec<u64>,
        structure: Vec<u64>,
        search: Vec<u64>,
        sql_elapsed: Duration,
        structure_elapsed: Duration,
        search_elapsed: Duration,
    }

    impl StageSamples {
        fn new() -> Self {
            Self {
                sql: Vec::with_capacity(STAGE_OBSERVATIONS),
                structure: Vec::with_capacity(STAGE_OBSERVATIONS),
                search: Vec::with_capacity(STAGE_OBSERVATIONS),
                sql_elapsed: Duration::ZERO,
                structure_elapsed: Duration::ZERO,
                search_elapsed: Duration::ZERO,
            }
        }

        fn finish(self) -> Result<StageStats, TestError> {
            Ok(StageStats {
                sql: Stats::from_samples(self.sql, self.sql_elapsed)?,
                structure: Stats::from_samples(self.structure, self.structure_elapsed)?,
                search: Stats::from_samples(self.search, self.search_elapsed)?,
            })
        }
    }

    fn measure_stage_iteration(
        client: &mut TransactionClient,
        sequence: u64,
        samples: &mut StageSamples,
    ) -> Result<(), TestError> {
        let begun = client.begin(DurabilityClass::Memory)?;
        let started = Instant::now();
        let sql = client.stage_sql(begun.handle, sequence)?;
        let (nanos, elapsed) = elapsed_nanos(started)?;
        samples.sql.push(nanos);
        samples.sql_elapsed += elapsed;
        require_stage_receipt(sql, begun.handle, LocalTransactionEngine::Relational, 1)?;

        let started = Instant::now();
        let structure = client.stage_structure(begun.handle, sequence)?;
        let (nanos, elapsed) = elapsed_nanos(started)?;
        samples.structure.push(nanos);
        samples.structure_elapsed += elapsed;
        require_stage_receipt(
            structure,
            begun.handle,
            LocalTransactionEngine::Structure,
            2,
        )?;

        let started = Instant::now();
        let search = client.stage_search(begun.handle, sequence)?;
        let (nanos, elapsed) = elapsed_nanos(started)?;
        samples.search.push(nanos);
        samples.search_elapsed += elapsed;
        require_stage_receipt(search, begun.handle, LocalTransactionEngine::Search, 3)?;
        client.rollback(begun.handle)?;
        Ok(())
    }

    fn measure_stages(data: &Path, socket: &Path) -> Result<StageStats, TestError> {
        let database = seed_database(data)?;
        let (mut client, server) = start_server(database, socket)?;
        for sequence in 0..STAGE_WARMUP {
            let sequence = sequence.checked_add(1).ok_or("sequence overflow")?;
            stage_once(&mut client, u64::try_from(sequence)?)?;
        }
        let mut samples = StageSamples::new();
        for sequence in 0..STAGE_OBSERVATIONS {
            let sequence = STAGE_WARMUP
                .checked_add(sequence)
                .and_then(|value| value.checked_add(1))
                .ok_or("sequence overflow")?;
            measure_stage_iteration(&mut client, u64::try_from(sequence)?, &mut samples)?;
        }
        client.close()?;
        join_server(server)?;
        samples.finish()
    }

    fn commit_once(
        client: &mut TransactionClient,
        durability: DurabilityClass,
        sequence: u64,
    ) -> Result<LocalTransactionCommitReceipt, TestError> {
        let begun = prepare_commit_transaction(client, durability, sequence)?;
        let receipt = client.commit(begun.handle)?;
        if receipt.handle != begun.handle
            || receipt.durability != durability
            || receipt.staged_operations != 3
        {
            return Err("benchmark commit receipt diverged".into());
        }
        Ok(receipt)
    }

    fn measure_commits(
        data: &Path,
        socket: &Path,
        durability: DurabilityClass,
        warmup: usize,
        observations: usize,
    ) -> Result<Stats, TestError> {
        let database = seed_database(data)?;
        let (mut client, server) = start_server(database, socket)?;
        let mut sequence = 1_u64;
        for _ in 0..warmup {
            commit_once(&mut client, durability, sequence)?;
            sequence = sequence.checked_add(1).ok_or("sequence overflow")?;
        }
        let mut samples = Vec::with_capacity(observations);
        let mut elapsed_total = Duration::ZERO;
        for _ in 0..observations {
            let begun = prepare_commit_transaction(&mut client, durability, sequence)?;
            let started = Instant::now();
            let receipt = client.commit(begun.handle)?;
            let (nanos, elapsed) = elapsed_nanos(started)?;
            if receipt.durability != durability || receipt.staged_operations != 3 {
                return Err("measured commit receipt diverged".into());
            }
            samples.push(nanos);
            elapsed_total += elapsed;
            sequence = sequence.checked_add(1).ok_or("sequence overflow")?;
        }
        client.close()?;
        join_server(server)?;
        Stats::from_samples(samples, elapsed_total)
    }

    fn measure_ping(data: &Path, socket: &Path) -> Result<Stats, TestError> {
        let database = seed_database(data)?;
        let (mut client, server) = start_server(database, socket)?;
        let payload = [0xa5; PING_BYTES];
        for _ in 0..PING_WARMUP {
            client.ping(&payload)?;
        }
        let mut samples = Vec::with_capacity(PING_OBSERVATIONS);
        let mut elapsed_total = Duration::ZERO;
        for _ in 0..PING_OBSERVATIONS {
            let started = Instant::now();
            client.ping(&payload)?;
            let (nanos, elapsed) = elapsed_nanos(started)?;
            samples.push(nanos);
            elapsed_total += elapsed;
        }
        client.close()?;
        join_server(server)?;
        Stats::from_samples(samples, elapsed_total)
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
        implementation_commit: &'value str,
        harness_commit: &'value str,
        ping: Stats,
        stages: StageStats,
        memory_commit: Stats,
        strict_commit: Stats,
    }

    fn print_receipt(receipt: &Receipt<'_>) {
        println!("{{");
        println!("  \"schema\": \"hyphae.native.local-all-engine-transaction-smoke.v1\",");
        println!("  \"status\": \"observation-not-regression-gate\",");
        println!(
            "  \"implementation_commit\": \"{}\",",
            receipt.implementation_commit
        );
        println!("  \"harness_commit\": \"{}\",", receipt.harness_commit);
        println!("  \"target\": \"x86_64-linux\",");
        println!("  \"profile\": \"release\",");
        println!("  \"concurrency\": 1,");
        println!("  \"warm_state\": true,");
        println!("  \"maximum_payload_bytes\": {MAXIMUM_PAYLOAD},");
        println!("  \"ping_warmup\": {PING_WARMUP},");
        println!("  \"ping_observations\": {PING_OBSERVATIONS},");
        println!("  \"stage_warmup\": {STAGE_WARMUP},");
        println!("  \"stage_observations\": {STAGE_OBSERVATIONS},");
        println!("  \"memory_commit_warmup\": {MEMORY_COMMIT_WARMUP},");
        println!("  \"memory_commit_observations\": {MEMORY_COMMIT_OBSERVATIONS},");
        println!("  \"strict_commit_warmup\": {STRICT_COMMIT_WARMUP},");
        println!("  \"strict_commit_observations\": {STRICT_COMMIT_OBSERVATIONS},");
        println!("  \"staged_operations_per_transaction\": 3,");
        println!("  \"commit_relational_identity_growth\": false,");
        println!("  \"commit_structure_identity_growth\": false,");
        println!("  \"commit_lexical_document_growth\": true,");
        println!("  \"operations\": {{");
        print_stats("persistent_ping_round_trip_32b", receipt.ping, true);
        print_stats(
            "persistent_transaction_sql_stage_round_trip",
            receipt.stages.sql,
            true,
        );
        print_stats(
            "persistent_transaction_structure_stage_round_trip",
            receipt.stages.structure,
            true,
        );
        print_stats(
            "persistent_transaction_search_stage_round_trip",
            receipt.stages.search,
            true,
        );
        print_stats(
            "persistent_transaction_memory_commit_round_trip",
            receipt.memory_commit,
            true,
        );
        print_stats(
            "persistent_transaction_strict_commit_round_trip",
            receipt.strict_commit,
            false,
        );
        println!("  }}");
        println!("}}");
    }

    fn argument(position: usize) -> String {
        std::env::args()
            .nth(position)
            .unwrap_or_else(|| "unknown".to_owned())
    }

    pub(super) fn run() -> Result<(), TestError> {
        let implementation_commit = argument(1);
        let harness_commit = argument(2);
        let temporary = TemporaryDirectory::create()?;
        let stages = measure_stages(
            &temporary.path().join("stage-data"),
            &temporary.path().join("stage.sock"),
        )?;
        let memory_commit = measure_commits(
            &temporary.path().join("memory-data"),
            &temporary.path().join("memory.sock"),
            DurabilityClass::Memory,
            MEMORY_COMMIT_WARMUP,
            MEMORY_COMMIT_OBSERVATIONS,
        )?;
        let strict_commit = measure_commits(
            &temporary.path().join("strict-data"),
            &temporary.path().join("strict.sock"),
            DurabilityClass::Strict,
            STRICT_COMMIT_WARMUP,
            STRICT_COMMIT_OBSERVATIONS,
        )?;
        let ping = measure_ping(
            &temporary.path().join("ping-data"),
            &temporary.path().join("ping.sock"),
        )?;
        print_receipt(&Receipt {
            implementation_commit: &implementation_commit,
            harness_commit: &harness_commit,
            ping,
            stages,
            memory_commit,
            strict_commit,
        });
        Ok(())
    }
}

#[cfg(unix)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    unix::run()
}
