// SPDX-License-Identifier: Apache-2.0

//! Direct-Linux native structure SET and TTL transport observations.

#[cfg(not(unix))]
fn main() {
    eprintln!("uds_structure_set_ttl_smoke is available only on Unix targets");
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
        FrameKind, LocalStructureCommitReceipt, LocalStructureSession, LocalTtlValue, LocalValue,
        NativeDatabase, NativeSchedulerClock, UdsFrameConnection, UdsFrameListener,
        decode_local_structure_commit_receipt, decode_local_ttl, decode_local_value,
        encode_local_structure_get, encode_local_structure_set, encode_local_structure_ttl,
    };
    use hyphae_native_types::{Csn, DurabilityClass, TransactionId};

    const MAXIMUM_PAYLOAD: usize = 128;
    const KEY_COUNT: u32 = 2_048;
    const TARGET_KEY: u32 = 1_024;
    const VALUE_BYTES: usize = 64;
    const READ_OBSERVATIONS: usize = 100_000;
    const READ_WARMUP: usize = 10_000;
    const MEMORY_SET_OBSERVATIONS: usize = 10_000;
    const MEMORY_SET_WARMUP: usize = 1_000;
    const STRICT_SET_OBSERVATIONS: usize = 256;
    const STRICT_SET_WARMUP: usize = 16;
    const STREAM_ID: u32 = 7;
    const HELLO_REQUEST_ID: u64 = 1;
    const LOGICAL_TIME_MICROS: i64 = 1_000_000;
    const RELATIVE_TTL_MICROS: i64 = 60_000_000;

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn create() -> Result<Self, Box<dyn std::error::Error>> {
            let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let path =
                Path::new("/tmp").join(format!("hy-set-smoke-{}-{timestamp}", std::process::id()));
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

    fn percentile(samples: &[u64], per_mille: usize) -> u64 {
        let index = samples.len().saturating_sub(1).saturating_mul(per_mille) / 1_000;
        samples[index]
    }

    fn measure<E>(
        observations: usize,
        mut operation: impl FnMut() -> Result<(), E>,
    ) -> Result<Stats, Box<dyn std::error::Error>>
    where
        E: std::error::Error + 'static,
    {
        let mut samples = Vec::with_capacity(observations);
        let total = Instant::now();
        for _ in 0..observations {
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
            maximum_nanos: *samples.last().ok_or("latency smoke produced no samples")?,
            throughput_per_second: f64::from(completed) / elapsed.as_secs_f64(),
        })
    }

    fn key(sequence: u32) -> [u8; 4] {
        sequence.to_be_bytes()
    }

    fn value(sequence: u64) -> [u8; VALUE_BYTES] {
        let mut value = [0_u8; VALUE_BYTES];
        let identity = sequence.to_le_bytes();
        for (offset, byte) in value.iter_mut().enumerate() {
            *byte = identity[offset % identity.len()] ^ u8::try_from(offset).unwrap_or(u8::MAX);
        }
        value
    }

    fn prepare_read_database(
        directory: &Path,
    ) -> Result<(NativeDatabase, String, usize), Box<dyn std::error::Error>> {
        let mut database = NativeDatabase::create(directory)?;
        let mut transaction = database.begin(LOGICAL_TIME_MICROS, DurabilityClass::Memory)?;
        let mut dataset = blake3::Hasher::new();
        for sequence in 0..KEY_COUNT {
            let key = key(sequence);
            let value = value(u64::from(sequence));
            dataset.update(&key);
            dataset.update(&value);
            let expiry =
                (sequence == TARGET_KEY).then_some(LOGICAL_TIME_MICROS + RELATIVE_TTL_MICROS);
            transaction.set(key.to_vec(), value.to_vec(), expiry)?;
        }
        transaction.commit()?;
        let tree_height = database.latest_structure_tree_height()?;
        if tree_height < 2 {
            return Err("SET/TTL smoke did not build a multilevel B+tree".into());
        }
        Ok((
            database,
            dataset.finalize().to_hex().to_string(),
            tree_height,
        ))
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
            LocalStructureSession::new(&mut database, &clock).serve(&mut connection)?;
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

    fn close(
        connection: &mut UdsFrameConnection,
        server: ServerHandle,
        request_id: u64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        connection.send(FrameKind::Close, 0, request_id, b"")?;
        let close = connection.receive()?.ok_or("server closed before CLOSE")?;
        if close.kind != FrameKind::Close
            || close.stream_id != 0
            || close.request_id != request_id
            || !close.payload.is_empty()
        {
            return Err("CLOSE response diverged".into());
        }
        server
            .join()
            .map_err(|_| std::io::Error::other("structure server panicked"))?
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        Ok(())
    }

    fn require_get(
        connection: &mut UdsFrameConnection,
        request_id: u64,
        request: &[u8],
        expected: &[u8],
    ) -> Result<(), Box<dyn std::error::Error>> {
        connection.send(FrameKind::Structure, STREAM_ID, request_id, request)?;
        let frame = connection
            .receive()?
            .ok_or("server closed before STRUCTURE GET")?;
        if frame.kind != FrameKind::Value
            || frame.stream_id != STREAM_ID
            || frame.request_id != request_id
            || decode_local_value(frame.payload)? != LocalValue::Present(expected)
        {
            return Err("STRUCTURE GET response diverged".into());
        }
        Ok(())
    }

    fn require_ttl(
        connection: &mut UdsFrameConnection,
        request_id: u64,
        request: &[u8],
    ) -> Result<(), Box<dyn std::error::Error>> {
        connection.send(FrameKind::Structure, STREAM_ID, request_id, request)?;
        let frame = connection
            .receive()?
            .ok_or("server closed before STRUCTURE TTL")?;
        if frame.kind != FrameKind::Value
            || frame.stream_id != STREAM_ID
            || frame.request_id != request_id
            || decode_local_ttl(frame.payload)?
                != LocalTtlValue::RemainingMicros(RELATIVE_TTL_MICROS)
        {
            return Err("STRUCTURE TTL response diverged".into());
        }
        Ok(())
    }

    #[derive(Clone, Copy)]
    struct SetCall<'value> {
        request_id: u64,
        commit_sequence: u64,
        value: &'value [u8],
        durability: DurabilityClass,
    }

    fn require_set(
        connection: &mut UdsFrameConnection,
        request_buffer: &mut Vec<u8>,
        call: SetCall<'_>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let request = encode_local_structure_set(
            request_buffer,
            b"set-key",
            call.value,
            Some(RELATIVE_TTL_MICROS),
            call.durability,
            MAXIMUM_PAYLOAD,
        )?;
        connection.send(FrameKind::Structure, STREAM_ID, call.request_id, request)?;
        let frame = connection
            .receive()?
            .ok_or("server closed before STRUCTURE SET")?;
        let expected = LocalStructureCommitReceipt {
            transaction_id: TransactionId::new(u128::from(call.commit_sequence))?,
            commit_csn: Csn::new(call.commit_sequence)?,
            durability: call.durability,
        };
        if frame.kind != FrameKind::Receipt
            || frame.stream_id != STREAM_ID
            || frame.request_id != call.request_id
            || decode_local_structure_commit_receipt(frame.payload)? != expected
        {
            return Err("STRUCTURE SET receipt diverged".into());
        }
        Ok(())
    }

    struct ReadStats {
        physical_ttl: Stats,
        get: Stats,
        ttl: Stats,
    }

    fn measure_read_surfaces(
        database: NativeDatabase,
        socket: &Path,
        target_key: &[u8],
        target_value: &[u8],
    ) -> Result<ReadStats, Box<dyn std::error::Error>> {
        for _ in 0..READ_WARMUP {
            black_box(database.ttl_latest_structure(target_key, LOGICAL_TIME_MICROS)?);
        }
        let physical_ttl = measure(READ_OBSERVATIONS, || {
            let ttl = database
                .ttl_latest_structure(black_box(target_key), LOGICAL_TIME_MICROS)
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            if black_box(ttl) != hyphae_native_runtime::Ttl::RemainingMicros(RELATIVE_TTL_MICROS) {
                return Err(std::io::Error::other("physical TTL response diverged"));
            }
            Ok(())
        })?;

        let (mut connection, server) = start_server(database, socket)?;
        handshake(&mut connection)?;
        let mut get_buffer = Vec::new();
        let get_request = encode_local_structure_get(&mut get_buffer, target_key)?.to_vec();
        let mut ttl_buffer = Vec::new();
        let ttl_request = encode_local_structure_ttl(&mut ttl_buffer, target_key)?.to_vec();
        let mut request_id = HELLO_REQUEST_ID + 1;
        for _ in 0..READ_WARMUP {
            require_get(&mut connection, request_id, &get_request, target_value)?;
            request_id += 1;
        }
        for _ in 0..READ_WARMUP {
            require_ttl(&mut connection, request_id, &ttl_request)?;
            request_id += 1;
        }
        let get = measure(READ_OBSERVATIONS, || {
            let current = request_id;
            request_id += 1;
            require_get(&mut connection, current, &get_request, target_value)
                .map_err(|error| std::io::Error::other(error.to_string()))
        })?;
        let ttl = measure(READ_OBSERVATIONS, || {
            let current = request_id;
            request_id += 1;
            require_ttl(&mut connection, current, &ttl_request)
                .map_err(|error| std::io::Error::other(error.to_string()))
        })?;
        close(&mut connection, server, request_id)?;
        Ok(ReadStats {
            physical_ttl,
            get,
            ttl,
        })
    }

    fn measure_set_surface(
        data: &Path,
        socket: &Path,
        durability: DurabilityClass,
        warmup: usize,
        observations: usize,
    ) -> Result<Stats, Box<dyn std::error::Error>> {
        let database = NativeDatabase::create(data)?;
        let (mut connection, server) = start_server(database, socket)?;
        handshake(&mut connection)?;
        let mut request_buffer = Vec::new();
        let mut commit_sequence = 1_u64;
        for _ in 0..warmup {
            let value = value(commit_sequence);
            require_set(
                &mut connection,
                &mut request_buffer,
                SetCall {
                    request_id: commit_sequence + HELLO_REQUEST_ID,
                    commit_sequence,
                    value: &value,
                    durability,
                },
            )?;
            commit_sequence += 1;
        }
        let stats = measure(observations, || {
            let value = value(commit_sequence);
            require_set(
                &mut connection,
                &mut request_buffer,
                SetCall {
                    request_id: commit_sequence + HELLO_REQUEST_ID,
                    commit_sequence,
                    value: &value,
                    durability,
                },
            )
            .map_err(|error| std::io::Error::other(error.to_string()))?;
            commit_sequence += 1;
            Ok::<(), std::io::Error>(())
        })?;
        close(&mut connection, server, commit_sequence + HELLO_REQUEST_ID)?;
        Ok(stats)
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
        read: ReadStats,
        memory_set: Stats,
        strict_set: Stats,
    }

    fn print_receipt(receipt: &Receipt<'_>) {
        println!("{{");
        println!("  \"schema\": \"hyphae.native.local-structure-set-ttl-smoke.v1\",");
        println!("  \"status\": \"observation-not-regression-gate\",");
        println!("  \"commit\": \"{}\",", receipt.commit);
        println!("  \"harness_commit\": \"{}\",", receipt.harness_commit);
        println!("  \"target\": \"x86_64-linux\",");
        println!("  \"profile\": \"release\",");
        println!("  \"concurrency\": 1,");
        println!("  \"warm_state\": true,");
        println!("  \"maximum_payload_bytes\": {MAXIMUM_PAYLOAD},");
        println!("  \"structure_keys\": {KEY_COUNT},");
        println!("  \"structure_tree_height\": {},", receipt.tree_height);
        println!("  \"key_bytes\": 4,");
        println!("  \"value_bytes\": {VALUE_BYTES},");
        println!("  \"relative_ttl_micros\": {RELATIVE_TTL_MICROS},");
        println!("  \"read_warmup\": {READ_WARMUP},");
        println!("  \"read_observations\": {READ_OBSERVATIONS},");
        println!("  \"memory_set_warmup\": {MEMORY_SET_WARMUP},");
        println!("  \"memory_set_observations\": {MEMORY_SET_OBSERVATIONS},");
        println!("  \"strict_set_warmup\": {STRICT_SET_WARMUP},");
        println!("  \"strict_set_observations\": {STRICT_SET_OBSERVATIONS},");
        println!(
            "  \"dataset_digest_blake3\": \"{}\",",
            receipt.dataset_digest
        );
        println!("  \"operations\": {{");
        print_stats("embedded_physical_ttl", receipt.read.physical_ttl, true);
        print_stats(
            "persistent_structure_get_round_trip_64b",
            receipt.read.get,
            true,
        );
        print_stats(
            "persistent_structure_ttl_round_trip",
            receipt.read.ttl,
            true,
        );
        print_stats(
            "persistent_structure_set_memory_round_trip_64b",
            receipt.memory_set,
            true,
        );
        print_stats(
            "persistent_structure_set_strict_round_trip_64b",
            receipt.strict_set,
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
        let (database, dataset_digest, tree_height) =
            prepare_read_database(&temporary.path().join("read-data"))?;
        let target_key = key(TARGET_KEY);
        let target_value = value(u64::from(TARGET_KEY));
        let read = measure_read_surfaces(
            database,
            &temporary.path().join("read.sock"),
            &target_key,
            &target_value,
        )?;
        let memory_set = measure_set_surface(
            &temporary.path().join("memory-data"),
            &temporary.path().join("memory.sock"),
            DurabilityClass::Memory,
            MEMORY_SET_WARMUP,
            MEMORY_SET_OBSERVATIONS,
        )?;
        let strict_set = measure_set_surface(
            &temporary.path().join("strict-data"),
            &temporary.path().join("strict.sock"),
            DurabilityClass::Strict,
            STRICT_SET_WARMUP,
            STRICT_SET_OBSERVATIONS,
        )?;
        print_receipt(&Receipt {
            commit: &commit,
            harness_commit: &harness_commit,
            dataset_digest: &dataset_digest,
            tree_height,
            read,
            memory_set,
            strict_set,
        });
        Ok(())
    }
}

#[cfg(unix)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    unix::run()
}
