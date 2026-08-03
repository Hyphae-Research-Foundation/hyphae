// SPDX-License-Identifier: Apache-2.0

//! Direct-Linux Unix-domain transport latency observation.

#[cfg(not(unix))]
fn main() {
    eprintln!("uds_ping_smoke is available only on Unix targets");
}

#[cfg(unix)]
mod unix {
    use std::{
        fs,
        path::{Path, PathBuf},
        thread,
        time::{Instant, SystemTime, UNIX_EPOCH},
    };

    use hyphae_native_runtime::{
        DecodedFrame, FrameKind, LocalTransportError, UdsFrameConnection, UdsFrameListener,
    };

    const MAXIMUM_PAYLOAD: usize = 64;
    const PING_PAYLOAD_BYTES: usize = 32;
    const HANDSHAKE_OBSERVATIONS: usize = 256;
    const PING_OBSERVATIONS: usize = 100_000;
    const PING_WARMUP: usize = 10_000;
    const SESSION_STREAM_ID: u32 = 7;
    const HELLO_REQUEST_ID: u64 = 1;
    const FIRST_PING_REQUEST_ID: u64 = 2;

    struct TemporaryDirectory(PathBuf);

    impl TemporaryDirectory {
        fn create() -> Result<Self, Box<dyn std::error::Error>> {
            let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let path = std::env::temp_dir().join(format!(
                "hyphae-native-local-uds-smoke-{}-{timestamp}",
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

    fn summarize(
        mut samples: Vec<u64>,
        elapsed: std::time::Duration,
    ) -> Result<Stats, Box<dyn std::error::Error>> {
        samples.sort_unstable();
        let completed = u32::try_from(samples.len())?;
        Ok(Stats {
            p50_nanos: percentile(&samples, 500),
            p95_nanos: percentile(&samples, 950),
            p99_nanos: percentile(&samples, 990),
            p999_nanos: percentile(&samples, 999),
            maximum_nanos: *samples.last().ok_or("UDS benchmark produced no samples")?,
            throughput_per_second: f64::from(completed) / elapsed.as_secs_f64(),
        })
    }

    fn require_frame(
        frame: Option<DecodedFrame<'_>>,
        expected_kind: FrameKind,
        expected_stream_id: u32,
        expected_request_id: u64,
        expected_payload: &[u8],
    ) -> Result<(), LocalTransportError> {
        let frame = frame.ok_or(LocalTransportError::Truncated)?;
        if frame.kind != expected_kind
            || frame.stream_id != expected_stream_id
            || frame.request_id != expected_request_id
            || frame.payload != expected_payload
        {
            return Err(
                std::io::Error::other("UDS receipt session received an unexpected frame").into(),
            );
        }
        Ok(())
    }

    fn serve_handshake_connection(
        connection: &mut UdsFrameConnection,
    ) -> Result<(), LocalTransportError> {
        require_frame(
            connection.receive()?,
            FrameKind::Hello,
            0,
            HELLO_REQUEST_ID,
            b"",
        )?;
        connection.send(FrameKind::Welcome, 0, HELLO_REQUEST_ID, b"")?;
        require_frame(connection.receive()?, FrameKind::Close, 0, 2, b"")?;
        connection.send(FrameKind::Close, 0, 2, b"")
    }

    fn serve_ping_connection(
        connection: &mut UdsFrameConnection,
        payload: &[u8; PING_PAYLOAD_BYTES],
    ) -> Result<(), LocalTransportError> {
        require_frame(
            connection.receive()?,
            FrameKind::Hello,
            0,
            HELLO_REQUEST_ID,
            b"",
        )?;
        connection.send(FrameKind::Welcome, 0, HELLO_REQUEST_ID, b"")?;
        let ping_count = PING_WARMUP
            .checked_add(PING_OBSERVATIONS)
            .ok_or(LocalTransportError::MaximumPayloadTooLarge)?;
        for offset in 0..ping_count {
            let request_id = FIRST_PING_REQUEST_ID
                .checked_add(
                    u64::try_from(offset)
                        .map_err(|_| LocalTransportError::MaximumPayloadTooLarge)?,
                )
                .ok_or(LocalTransportError::MaximumPayloadTooLarge)?;
            require_frame(
                connection.receive()?,
                FrameKind::Ping,
                SESSION_STREAM_ID,
                request_id,
                payload,
            )?;
            connection.send(FrameKind::Ping, SESSION_STREAM_ID, request_id, payload)?;
        }
        let close_request_id = FIRST_PING_REQUEST_ID
            .checked_add(
                u64::try_from(ping_count)
                    .map_err(|_| LocalTransportError::MaximumPayloadTooLarge)?,
            )
            .ok_or(LocalTransportError::MaximumPayloadTooLarge)?;
        require_frame(
            connection.receive()?,
            FrameKind::Close,
            0,
            close_request_id,
            b"",
        )?;
        connection.send(FrameKind::Close, 0, close_request_id, b"")
    }

    fn connect_and_handshake(socket: &Path) -> Result<UdsFrameConnection, LocalTransportError> {
        let mut connection = UdsFrameConnection::connect(socket, MAXIMUM_PAYLOAD)?;
        connection.send(FrameKind::Hello, 0, HELLO_REQUEST_ID, b"")?;
        require_frame(
            connection.receive()?,
            FrameKind::Welcome,
            0,
            HELLO_REQUEST_ID,
            b"",
        )?;
        Ok(connection)
    }

    fn finish_handshake_session(
        mut connection: UdsFrameConnection,
    ) -> Result<(), LocalTransportError> {
        connection.send(FrameKind::Close, 0, 2, b"")?;
        require_frame(connection.receive()?, FrameKind::Close, 0, 2, b"")
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

    pub(super) fn run() -> Result<(), Box<dyn std::error::Error>> {
        let commit = std::env::args()
            .nth(1)
            .unwrap_or_else(|| "unknown".to_owned());
        let harness_commit = std::env::args()
            .nth(2)
            .unwrap_or_else(|| "unknown".to_owned());
        let temporary = TemporaryDirectory::create()?;
        let socket = temporary.path().join("hyphae.sock");
        let listener = UdsFrameListener::bind(&socket, MAXIMUM_PAYLOAD)?;
        let payload = [0xa5; PING_PAYLOAD_BYTES];

        let server = thread::spawn(move || -> Result<(), LocalTransportError> {
            for _ in 0..HANDSHAKE_OBSERVATIONS {
                let mut connection = listener.accept()?;
                serve_handshake_connection(&mut connection)?;
            }
            let mut connection = listener.accept()?;
            serve_ping_connection(&mut connection, &payload)?;
            listener.close()
        });

        let mut handshake_samples = Vec::with_capacity(HANDSHAKE_OBSERVATIONS);
        let handshake_total = Instant::now();
        for _ in 0..HANDSHAKE_OBSERVATIONS {
            let started = Instant::now();
            let connection = connect_and_handshake(&socket)?;
            finish_handshake_session(connection)?;
            handshake_samples.push(u64::try_from(started.elapsed().as_nanos())?);
        }
        let handshake_elapsed = handshake_total.elapsed();

        let mut connection = connect_and_handshake(&socket)?;
        for offset in 0..PING_WARMUP {
            let request_id = FIRST_PING_REQUEST_ID + u64::try_from(offset)?;
            connection.send(FrameKind::Ping, SESSION_STREAM_ID, request_id, &payload)?;
            require_frame(
                connection.receive()?,
                FrameKind::Ping,
                SESSION_STREAM_ID,
                request_id,
                &payload,
            )?;
        }

        let mut ping_samples = Vec::with_capacity(PING_OBSERVATIONS);
        let ping_total = Instant::now();
        for offset in 0..PING_OBSERVATIONS {
            let absolute_offset = PING_WARMUP
                .checked_add(offset)
                .ok_or(std::io::Error::other("UDS benchmark request ID overflow"))?;
            let request_id = FIRST_PING_REQUEST_ID + u64::try_from(absolute_offset)?;
            let started = Instant::now();
            connection.send(FrameKind::Ping, SESSION_STREAM_ID, request_id, &payload)?;
            let response = connection.receive()?;
            ping_samples.push(u64::try_from(started.elapsed().as_nanos())?);
            require_frame(
                response,
                FrameKind::Ping,
                SESSION_STREAM_ID,
                request_id,
                &payload,
            )?;
        }
        let ping_elapsed = ping_total.elapsed();
        let close_request_id =
            FIRST_PING_REQUEST_ID + u64::try_from(PING_WARMUP + PING_OBSERVATIONS)?;
        connection.send(FrameKind::Close, 0, close_request_id, b"")?;
        require_frame(
            connection.receive()?,
            FrameKind::Close,
            0,
            close_request_id,
            b"",
        )?;
        server
            .join()
            .map_err(|_| std::io::Error::other("UDS receipt server panicked"))??;

        let handshake = summarize(handshake_samples, handshake_elapsed)?;
        let ping = summarize(ping_samples, ping_elapsed)?;
        println!("{{");
        println!("  \"schema\": \"hyphae.native.local-uds-smoke.v1\",");
        println!("  \"status\": \"observation-not-universal-slo\",");
        println!("  \"commit\": \"{commit}\",");
        println!("  \"harness_commit\": \"{harness_commit}\",");
        println!("  \"target\": \"x86_64-linux\",");
        println!("  \"profile\": \"release\",");
        println!("  \"concurrency\": 1,");
        println!("  \"socket_mode\": \"0600\",");
        println!("  \"maximum_payload_bytes\": {MAXIMUM_PAYLOAD},");
        println!("  \"ping_payload_bytes\": {PING_PAYLOAD_BYTES},");
        println!("  \"handshake_observations\": {HANDSHAKE_OBSERVATIONS},");
        println!("  \"ping_observations\": {PING_OBSERVATIONS},");
        println!("  \"ping_warmup\": {PING_WARMUP},");
        println!("  \"operations\": {{");
        print_stats("connect_handshake_close", handshake, true);
        print_stats("persistent_ping_round_trip", ping, false);
        println!("  }}");
        println!("}}");
        Ok(())
    }
}

#[cfg(unix)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    unix::run()
}
