// SPDX-License-Identifier: AGPL-3.0-only

//! Direct-Linux native structure GET transport latency observation.

#[cfg(not(unix))]
fn main() {
    eprintln!("uds_structure_get_smoke is available only on Unix targets");
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
        FrameKind, LocalDataSession, LocalValue, NativeDatabase, NativeSchedulerClock,
        UdsFrameConnection, UdsFrameListener, decode_local_value, encode_local_structure_get,
    };
    use hyphae_native_types::DurabilityClass;

    const MAXIMUM_PAYLOAD: usize = 128;
    const KEY_COUNT: u32 = 2_048;
    const TARGET_KEY: u32 = 1_024;
    const VALUE_BYTES: usize = 64;
    const PING_BYTES: usize = 32;
    const OBSERVATIONS: usize = 100_000;
    const WARMUP: usize = 10_000;
    const PING_STREAM_ID: u32 = 7;
    const GET_STREAM_ID: u32 = 8;
    const HELLO_REQUEST_ID: u64 = 1;
    const LOGICAL_TIME_MICROS: i64 = 101;

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn create() -> Result<Self, Box<dyn std::error::Error>> {
            let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let path = std::env::temp_dir().join(format!(
                "hyphae-local-structure-get-smoke-{}-{timestamp}",
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
            maximum_nanos: *samples
                .last()
                .ok_or("structure GET smoke produced no samples")?,
            throughput_per_second: f64::from(completed) / elapsed.as_secs_f64(),
        })
    }

    fn key(sequence: u32) -> [u8; 4] {
        sequence.to_be_bytes()
    }

    fn value(sequence: u32) -> [u8; VALUE_BYTES] {
        let mut value = [0_u8; VALUE_BYTES];
        let identity = sequence.to_le_bytes();
        for (offset, byte) in value.iter_mut().enumerate() {
            *byte = identity[offset % identity.len()] ^ u8::try_from(offset).unwrap_or(u8::MAX);
        }
        value
    }

    fn prepare_database(
        directory: &Path,
    ) -> Result<(NativeDatabase, String, usize), Box<dyn std::error::Error>> {
        let mut database = NativeDatabase::create(directory)?;
        let mut transaction = database.begin(100, DurabilityClass::Memory)?;
        let mut dataset = blake3::Hasher::new();
        for sequence in 0..KEY_COUNT {
            let key = key(sequence);
            let value = value(sequence);
            dataset.update(&key);
            dataset.update(&value);
            transaction.set(key.to_vec(), value.to_vec(), None)?;
        }
        transaction.commit()?;
        let tree_height = database.latest_structure_tree_height()?;
        if tree_height < 2 {
            return Err("structure GET smoke did not build a multilevel B+tree".into());
        }
        Ok((
            database,
            dataset.finalize().to_hex().to_string(),
            tree_height,
        ))
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

    fn require_get(
        connection: &mut UdsFrameConnection,
        request_id: u64,
        request: &[u8],
        expected: &[u8],
    ) -> Result<(), Box<dyn std::error::Error>> {
        connection.send(FrameKind::Structure, GET_STREAM_ID, request_id, request)?;
        let frame = connection
            .receive()?
            .ok_or("server closed before STRUCTURE GET")?;
        if frame.kind != FrameKind::Value
            || frame.stream_id != GET_STREAM_ID
            || frame.request_id != request_id
            || decode_local_value(frame.payload)? != LocalValue::Present(expected)
        {
            return Err("STRUCTURE GET response diverged".into());
        }
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

    fn measure_embedded(
        database: &NativeDatabase,
        target_key: &[u8],
        target_value: &[u8],
    ) -> Result<Stats, Box<dyn std::error::Error>> {
        for _ in 0..WARMUP {
            let observed =
                database.get_latest_structure(black_box(target_key), LOGICAL_TIME_MICROS)?;
            if observed.as_deref() != Some(target_value) {
                return Err("embedded structure GET warmup diverged".into());
            }
        }
        measure(|| {
            let observed = database
                .get_latest_structure(black_box(target_key), LOGICAL_TIME_MICROS)
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            if black_box(observed.as_deref()) != Some(target_value) {
                return Err(std::io::Error::other("embedded structure GET diverged"));
            }
            Ok(())
        })
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
        target_key: &[u8],
        target_value: &[u8],
    ) -> Result<(Stats, Stats), Box<dyn std::error::Error>> {
        handshake(&mut connection)?;
        let ping_payload = [0xa5; PING_BYTES];
        let mut request_buffer = Vec::new();
        let get_request = encode_local_structure_get(&mut request_buffer, target_key)?.to_vec();
        let mut next_request_id = HELLO_REQUEST_ID + 1;
        for _ in 0..WARMUP {
            require_ping(&mut connection, next_request_id, &ping_payload)?;
            next_request_id += 1;
        }
        for _ in 0..WARMUP {
            require_get(&mut connection, next_request_id, &get_request, target_value)?;
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
        let get = measure(|| {
            let request_id = next_request_id;
            next_request_id = next_request_id
                .checked_add(1)
                .ok_or_else(|| std::io::Error::other("GET request ID overflow"))?;
            require_get(&mut connection, request_id, &get_request, target_value)
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
            .map_err(|_| std::io::Error::other("structure GET server panicked"))?
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        Ok((ping, get))
    }

    struct Receipt<'value> {
        commit: &'value str,
        harness_commit: &'value str,
        dataset_digest: &'value str,
        tree_height: usize,
        embedded: Stats,
        ping: Stats,
        get: Stats,
    }

    fn print_receipt(receipt: &Receipt<'_>) {
        println!("{{");
        println!("  \"schema\": \"hyphae.native.local-structure-get-smoke.v1\",");
        println!("  \"status\": \"observation-not-regression-gate\",");
        println!("  \"commit\": \"{}\",", receipt.commit);
        println!("  \"harness_commit\": \"{}\",", receipt.harness_commit);
        println!("  \"target\": \"x86_64-linux\",");
        println!("  \"profile\": \"release\",");
        println!("  \"concurrency\": 1,");
        println!("  \"warm_state\": true,");
        println!("  \"durability\": \"memory\",");
        println!("  \"maximum_payload_bytes\": {MAXIMUM_PAYLOAD},");
        println!("  \"structure_keys\": {KEY_COUNT},");
        println!("  \"structure_tree_height\": {},", receipt.tree_height);
        println!("  \"value_bytes\": {VALUE_BYTES},");
        println!("  \"ping_bytes\": {PING_BYTES},");
        println!("  \"warmup_per_operation\": {WARMUP},");
        println!("  \"observations_per_operation\": {OBSERVATIONS},");
        println!(
            "  \"dataset_digest_blake3\": \"{}\",",
            receipt.dataset_digest
        );
        println!("  \"operations\": {{");
        print_stats(
            "embedded_physical_structure_get_64b",
            receipt.embedded,
            true,
        );
        print_stats("persistent_ping_round_trip_32b", receipt.ping, true);
        print_stats(
            "persistent_structure_get_round_trip_64b",
            receipt.get,
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
        let (database, dataset_digest, tree_height) = prepare_database(&data)?;
        let target_key = key(TARGET_KEY);
        let target_value = value(TARGET_KEY);
        let embedded = measure_embedded(&database, &target_key, &target_value)?;
        let (connection, server) = start_server(database, &socket)?;
        let (ping, get) = measure_remote(connection, server, &target_key, &target_value)?;
        print_receipt(&Receipt {
            commit: &commit,
            harness_commit: &harness_commit,
            dataset_digest: &dataset_digest,
            tree_height,
            embedded,
            ping,
            get,
        });
        Ok(())
    }
}

#[cfg(unix)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    unix::run()
}
