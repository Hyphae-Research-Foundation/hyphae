// SPDX-License-Identifier: Apache-2.0

// Exercises the deprecated pre-daemon local session/transport on purpose.
#![allow(deprecated)]

//! Direct-Linux native prepared SQL transport latency observation.

#[cfg(not(unix))]
fn main() {
    eprintln!("uds_sql_select_smoke is available only on Unix targets");
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
        FrameKind, LocalDataSession, NativeDatabase, NativeSchedulerClock, PreparedStatement,
        SqlResult, SqlValue, UdsFrameConnection, UdsFrameListener,
        decode_local_sql_prepared_receipt, decode_local_sql_rows, encode_local_sql_execute,
        encode_local_sql_prepare,
    };
    use hyphae_native_types::{Csn, DurabilityClass};

    const MAXIMUM_PAYLOAD: usize = 256;
    const ROW_COUNT: u32 = 2_048;
    const TARGET_ID: i64 = 1_024;
    const QUERY: &str = "SELECT id, payload FROM benchmark_people WHERE id = ?";
    const PING_BYTES: usize = 32;
    const OBSERVATIONS: usize = 100_000;
    const WARMUP: usize = 10_000;
    const PING_STREAM_ID: u32 = 7;
    const SQL_STREAM_ID: u32 = 9;
    const HELLO_REQUEST_ID: u64 = 1;
    const PREPARE_REQUEST_ID: u64 = 2;

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn create() -> Result<Self, Box<dyn std::error::Error>> {
            let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let path = std::env::temp_dir().join(format!(
                "hyphae-local-sql-select-smoke-{}-{timestamp}",
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
            maximum_nanos: *samples.last().ok_or("SQL smoke produced no samples")?,
            throughput_per_second: f64::from(completed) / elapsed.as_secs_f64(),
        })
    }

    fn payload(sequence: u32) -> Vec<u8> {
        let mut payload = vec![0xa5; 32];
        payload[..4].copy_from_slice(&sequence.to_le_bytes());
        payload
    }

    struct PreparedCorpus {
        database: NativeDatabase,
        prepared: PreparedStatement,
        parameters: Vec<SqlValue>,
        expected: SqlResult,
        dataset_digest: String,
        tree_height: usize,
    }

    fn prepare_database(directory: &Path) -> Result<PreparedCorpus, Box<dyn std::error::Error>> {
        let mut database = NativeDatabase::create(directory)?;
        let mut transaction = database.begin_sql(100, DurabilityClass::Memory)?;
        transaction.execute_sql(
            "CREATE TABLE benchmark_people (
                id BIGINT PRIMARY KEY,
                payload BINARY NOT NULL
            )",
            &[],
        )?;
        let mut dataset = blake3::Hasher::new();
        for sequence in 0..ROW_COUNT {
            let payload = payload(sequence);
            dataset.update(&sequence.to_le_bytes());
            dataset.update(&payload);
            transaction.execute_sql(
                "INSERT INTO benchmark_people (id, payload) VALUES (?, ?)",
                &[
                    SqlValue::Signed(i64::from(sequence)),
                    SqlValue::Binary(payload),
                ],
            )?;
        }
        let receipt = transaction.commit()?;
        if receipt.commit_csn != Csn::new(1)? {
            return Err("SQL smoke seed CSN diverged".into());
        }
        let tree_height = database.latest_relational_tree_height()?;
        if tree_height < 2 {
            return Err("SQL smoke did not build a multilevel B+tree".into());
        }
        let prepared = database.prepare_sql_latest(QUERY)?;
        let parameters = vec![SqlValue::Signed(TARGET_ID)];
        let expected = database.execute_prepared_latest(&prepared, &parameters)?;
        let SqlResult::Rows { columns, rows } = &expected else {
            return Err("SQL smoke did not produce rows".into());
        };
        if columns != &["id".to_owned(), "payload".to_owned()] || rows.len() != 1 {
            return Err("SQL smoke primary-key result diverged".into());
        }
        Ok(PreparedCorpus {
            database,
            prepared,
            parameters,
            expected,
            dataset_digest: dataset.finalize().to_hex().to_string(),
            tree_height,
        })
    }

    fn result_matches(observed: &SqlResult, expected: &SqlResult) -> bool {
        observed == expected
    }

    fn measure_embedded(
        database: &NativeDatabase,
        prepared: &PreparedStatement,
        parameters: &[SqlValue],
        expected: &SqlResult,
    ) -> Result<Stats, Box<dyn std::error::Error>> {
        for _ in 0..WARMUP {
            let observed = database.execute_prepared_latest(prepared, black_box(parameters))?;
            if !result_matches(&observed, expected) {
                return Err("embedded SQL warmup diverged".into());
            }
        }
        measure(|| {
            let observed = database
                .execute_prepared_latest(prepared, black_box(parameters))
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            if !result_matches(black_box(&observed), expected) {
                return Err(std::io::Error::other("embedded SQL result diverged"));
            }
            Ok(())
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

    fn require_execute(
        connection: &mut UdsFrameConnection,
        request_id: u64,
        request: &[u8],
        expected: &SqlResult,
    ) -> Result<usize, Box<dyn std::error::Error>> {
        connection.send(FrameKind::Execute, SQL_STREAM_ID, request_id, request)?;
        let frame = connection
            .receive()?
            .ok_or("server closed before SQL result")?;
        if frame.kind != FrameKind::Value
            || frame.stream_id != SQL_STREAM_ID
            || frame.request_id != request_id
        {
            return Err("SQL response identity diverged".into());
        }
        let decoded = decode_local_sql_rows(frame.payload)?;
        let SqlResult::Rows { columns, rows } = expected else {
            return Err("embedded SQL expectation is not rows".into());
        };
        if decoded.visible_csn != Csn::new(1)?
            || decoded
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .ne(columns.iter().map(String::as_str))
            || &decoded.rows != rows
        {
            return Err("SQL response content diverged".into());
        }
        Ok(frame.payload.len())
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

    fn handshake_and_prepare(
        connection: &mut UdsFrameConnection,
    ) -> Result<hyphae_native_runtime::LocalSqlPreparedReceipt, Box<dyn std::error::Error>> {
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
        let mut buffer = Vec::new();
        let request = encode_local_sql_prepare(&mut buffer, QUERY, MAXIMUM_PAYLOAD)?;
        connection.send(
            FrameKind::Prepare,
            SQL_STREAM_ID,
            PREPARE_REQUEST_ID,
            request,
        )?;
        let frame = connection
            .receive()?
            .ok_or("server closed before SQL PREPARE receipt")?;
        if frame.kind != FrameKind::Receipt
            || frame.stream_id != SQL_STREAM_ID
            || frame.request_id != PREPARE_REQUEST_ID
        {
            return Err("SQL PREPARE receipt identity diverged".into());
        }
        let receipt = decode_local_sql_prepared_receipt(frame.payload)?;
        if receipt.parameter_count != 1 || receipt.column_count != 2 || receipt.maximum_rows != 1 {
            return Err("SQL PREPARE metadata diverged".into());
        }
        Ok(receipt)
    }

    struct RemoteStats {
        ping: Stats,
        executed: Stats,
        request_bytes: usize,
        result_bytes: usize,
    }

    fn measure_remote(
        mut connection: UdsFrameConnection,
        server: ServerHandle,
        parameters: &[SqlValue],
        expected: &SqlResult,
    ) -> Result<RemoteStats, Box<dyn std::error::Error>> {
        let receipt = handshake_and_prepare(&mut connection)?;
        let ping_payload = [0xa5; PING_BYTES];
        let mut buffer = Vec::new();
        let execute_request =
            encode_local_sql_execute(&mut buffer, receipt.plan_id, parameters, MAXIMUM_PAYLOAD)?
                .to_vec();
        let mut next_request_id = PREPARE_REQUEST_ID + 1;
        let result_bytes =
            require_execute(&mut connection, next_request_id, &execute_request, expected)?;
        next_request_id += 1;
        for _ in 0..WARMUP {
            require_ping(&mut connection, next_request_id, &ping_payload)?;
            next_request_id += 1;
        }
        for _ in 0..WARMUP {
            require_execute(&mut connection, next_request_id, &execute_request, expected)?;
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
        let executed = measure(|| {
            let request_id = next_request_id;
            next_request_id = next_request_id
                .checked_add(1)
                .ok_or_else(|| std::io::Error::other("SQL request ID overflow"))?;
            require_execute(&mut connection, request_id, &execute_request, expected)
                .map(|_| ())
                .map_err(|error| std::io::Error::other(error.to_string()))
        })?;
        close(connection, server, next_request_id)?;
        Ok(RemoteStats {
            ping,
            executed,
            request_bytes: execute_request.len(),
            result_bytes,
        })
    }

    fn close(
        mut connection: UdsFrameConnection,
        server: ServerHandle,
        request_id: u64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        connection.send(FrameKind::Close, 0, request_id, b"")?;
        let frame = connection.receive()?.ok_or("server closed before CLOSE")?;
        if frame.kind != FrameKind::Close
            || frame.stream_id != 0
            || frame.request_id != request_id
            || !frame.payload.is_empty()
        {
            return Err("CLOSE response diverged".into());
        }
        server
            .join()
            .map_err(|_| std::io::Error::other("SQL server panicked"))?
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        Ok(())
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
        remote: RemoteStats,
    }

    fn print_receipt(receipt: &Receipt<'_>) {
        println!("{{");
        println!("  \"schema\": \"hyphae.native.local-sql-select-smoke.v1\",");
        println!("  \"status\": \"observation-not-regression-gate\",");
        println!("  \"commit\": \"{}\",", receipt.commit);
        println!("  \"harness_commit\": \"{}\",", receipt.harness_commit);
        println!("  \"target\": \"x86_64-linux\",");
        println!("  \"profile\": \"release\",");
        println!("  \"concurrency\": 1,");
        println!("  \"warm_state\": true,");
        println!("  \"durability\": \"memory\",");
        println!("  \"maximum_payload_bytes\": {MAXIMUM_PAYLOAD},");
        println!("  \"relational_rows\": {ROW_COUNT},");
        println!("  \"relational_tree_height\": {},", receipt.tree_height);
        println!("  \"query_bytes\": {},", QUERY.len());
        println!("  \"parameter_count\": 1,");
        println!("  \"result_columns\": 2,");
        println!("  \"result_rows\": 1,");
        println!(
            "  \"execute_request_bytes\": {},",
            receipt.remote.request_bytes
        );
        println!("  \"result_bytes\": {},", receipt.remote.result_bytes);
        println!("  \"ping_bytes\": {PING_BYTES},");
        println!("  \"warmup_per_operation\": {WARMUP},");
        println!("  \"observations_per_operation\": {OBSERVATIONS},");
        println!(
            "  \"dataset_digest_blake3\": \"{}\",",
            receipt.dataset_digest
        );
        println!("  \"operations\": {{");
        print_stats(
            "embedded_physical_prepared_primary_key_select",
            receipt.embedded,
            true,
        );
        print_stats("persistent_ping_round_trip_32b", receipt.remote.ping, true);
        print_stats(
            "persistent_sql_execute_round_trip_one_row",
            receipt.remote.executed,
            false,
        );
        println!("  }}");
        println!("}}");
    }

    fn argument(position: usize) -> String {
        match std::env::args().nth(position) {
            Some(value) => value,
            None => "unknown".to_owned(),
        }
    }

    pub(super) fn run() -> Result<(), Box<dyn std::error::Error>> {
        let commit = argument(1);
        let harness_commit = argument(2);
        let temporary = TemporaryDirectory::create()?;
        let data = temporary.path().join("data");
        let socket = temporary.path().join("hyphae.sock");
        let corpus = prepare_database(&data)?;
        let embedded = measure_embedded(
            &corpus.database,
            &corpus.prepared,
            &corpus.parameters,
            &corpus.expected,
        )?;
        let (connection, server) = start_server(corpus.database, &socket)?;
        let remote = measure_remote(connection, server, &corpus.parameters, &corpus.expected)?;
        print_receipt(&Receipt {
            commit: &commit,
            harness_commit: &harness_commit,
            dataset_digest: &corpus.dataset_digest,
            tree_height: corpus.tree_height,
            embedded,
            remote,
        });
        Ok(())
    }
}

#[cfg(unix)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    unix::run()
}
