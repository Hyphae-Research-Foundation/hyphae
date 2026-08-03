// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use hyphae_native_types::DurabilityClass;

use crate::{
    HashFieldEntry, HashPatternScanPage, HashPatternScanRequest, HashPatternScanStop,
    HashSetOutcome, NativeDatabase, NativeRuntimeError, NativeSnapshot, NativeWriteBatch, Ttl,
    model::{ModelError, StructureState, TtlValue},
};

const FIXED_SEEDS: [u64; 16] = [
    0x4859_5048_4145_0001,
    0x4859_5048_4145_0002,
    0x4859_5048_4145_0003,
    0x4859_5048_4145_0004,
    0x4859_5048_4145_0005,
    0x4859_5048_4145_0006,
    0x4859_5048_4145_0007,
    0x4859_5048_4145_0008,
    0x4859_5048_4145_0009,
    0x4859_5048_4145_000a,
    0x4859_5048_4145_000b,
    0x4859_5048_4145_000c,
    0x4859_5048_4145_000d,
    0x4859_5048_4145_000e,
    0x4859_5048_4145_000f,
    0x4859_5048_4145_0010,
];
const STEPS_PER_SEED: usize = 256;
const REOPEN_INTERVAL: usize = 32;
const HASH_KEY_COUNT: usize = 4;
const HASH_FIELD_COUNT: usize = 32;
const COMPLETE_SCAN_LIMIT: usize = 64;
const PATTERN_MATCH_STEP_LIMIT: usize = 16_384;
const ACTION_KIND_COUNT: usize = 11;

type HashEntries = Vec<(Vec<u8>, Vec<u8>)>;

#[derive(Clone, Debug)]
enum Action {
    CreateHash {
        key: Vec<u8>,
    },
    DeleteHash {
        key: Vec<u8>,
    },
    ExpireHash {
        key: Vec<u8>,
        expiry: i64,
    },
    Hset {
        key: Vec<u8>,
        field: Vec<u8>,
        value: Vec<u8>,
    },
    HsetMany {
        key: Vec<u8>,
        updates: Vec<(Vec<u8>, Vec<u8>)>,
    },
    Hdelete {
        key: Vec<u8>,
        field: Vec<u8>,
    },
    HdeleteMany {
        key: Vec<u8>,
        fields: Vec<Vec<u8>>,
    },
    Hincrement {
        key: Vec<u8>,
        field: Vec<u8>,
        delta: i64,
    },
    ExpireField {
        key: Vec<u8>,
        field: Vec<u8>,
        expiry: i64,
    },
    AdvanceTime {
        target: i64,
    },
    Probe {
        key: Vec<u8>,
        field: Vec<u8>,
    },
}

impl Action {
    const fn kind_index(&self) -> usize {
        match self {
            Self::CreateHash { .. } => 0,
            Self::DeleteHash { .. } => 1,
            Self::ExpireHash { .. } => 2,
            Self::Hset { .. } => 3,
            Self::HsetMany { .. } => 4,
            Self::Hdelete { .. } => 5,
            Self::HdeleteMany { .. } => 6,
            Self::Hincrement { .. } => 7,
            Self::ExpireField { .. } => 8,
            Self::AdvanceTime { .. } => 9,
            Self::Probe { .. } => 10,
        }
    }

    fn description(&self) -> String {
        match self {
            Self::CreateHash { key } => format!("create-hash key={}", hex(key)),
            Self::DeleteHash { key } => format!("delete-hash key={}", hex(key)),
            Self::ExpireHash { key, expiry } => {
                format!("expire-hash key={} expiry={expiry}", hex(key))
            }
            Self::Hset { key, field, value } => format!(
                "hset key={} field={} value={}",
                hex(key),
                hex(field),
                hex(value)
            ),
            Self::HsetMany { key, updates } => {
                let fields = updates
                    .iter()
                    .map(|(field, _)| hex(field))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("hset-many key={} fields=[{fields}]", hex(key))
            }
            Self::Hdelete { key, field } => {
                format!("hdelete key={} field={}", hex(key), hex(field))
            }
            Self::HdeleteMany { key, fields } => {
                let fields = fields.iter().map(|field| hex(field)).collect::<Vec<_>>();
                format!(
                    "hdelete-many key={} fields=[{}]",
                    hex(key),
                    fields.join(",")
                )
            }
            Self::Hincrement { key, field, delta } => format!(
                "hincrement key={} field={} delta={delta}",
                hex(key),
                hex(field)
            ),
            Self::ExpireField { key, field, expiry } => format!(
                "expire-field key={} field={} expiry={expiry}",
                hex(key),
                hex(field)
            ),
            Self::AdvanceTime { target } => format!("advance-time target={target}"),
            Self::Probe { key, field } => {
                format!("probe key={} field={}", hex(key), hex(field))
            }
        }
    }
}

#[derive(Clone, Copy)]
struct TraceContext<'action> {
    seed: u64,
    seed_ordinal: usize,
    step: usize,
    logical_time_micros: i64,
    action: &'action Action,
}

impl fmt::Display for TraceContext<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "seed=0x{:016x} seed_ordinal={} step={} logical_time={} action={}",
            self.seed,
            self.seed_ordinal,
            self.step,
            self.logical_time_micros,
            self.action.description()
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExpectedError {
    KeyExists,
    UnknownHash,
    ValueNotInteger,
    IntegerOverflow,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum CommandOutcome {
    Noop,
    Unit,
    Bool(bool),
    Count(usize),
    HashSet(HashSetOutcome),
    Integer(i64),
    Error(ExpectedError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PatternObservation {
    entries: Vec<(Vec<u8>, Vec<u8>)>,
    continuation: Option<Vec<u8>>,
    stop: HashPatternScanStop,
    visited: usize,
    match_steps: usize,
}

trait HashReadSurface {
    fn label(&self) -> &'static str;

    fn hash_ttl(&self, key: &[u8]) -> Result<Ttl, NativeRuntimeError>;

    fn field_ttl(&self, key: &[u8], field: &[u8]) -> Result<Ttl, NativeRuntimeError>;

    fn hget(&self, key: &[u8], field: &[u8]) -> Result<Option<Vec<u8>>, NativeRuntimeError>;

    fn hlen(&self, key: &[u8]) -> Result<usize, NativeRuntimeError>;

    fn hscan(
        &self,
        key: &[u8],
        start_after: Option<&[u8]>,
        limit: usize,
    ) -> Result<HashEntries, NativeRuntimeError>;

    fn hscan_reverse(
        &self,
        key: &[u8],
        start_before: Option<&[u8]>,
        limit: usize,
    ) -> Result<HashEntries, NativeRuntimeError>;

    fn hscan_match(
        &self,
        key: &[u8],
        request: &HashPatternScanRequest,
    ) -> Result<PatternObservation, NativeRuntimeError>;
}

impl HashReadSurface for NativeWriteBatch {
    fn label(&self) -> &'static str {
        "private"
    }

    fn hash_ttl(&self, key: &[u8]) -> Result<Ttl, NativeRuntimeError> {
        Ok(NativeWriteBatch::ttl_hash(self, key))
    }

    fn field_ttl(&self, key: &[u8], field: &[u8]) -> Result<Ttl, NativeRuntimeError> {
        Ok(NativeWriteBatch::ttl_hash_field(self, key, field))
    }

    fn hget(&self, key: &[u8], field: &[u8]) -> Result<Option<Vec<u8>>, NativeRuntimeError> {
        NativeWriteBatch::hget(self, key, field).map(|value| value.map(<[u8]>::to_vec))
    }

    fn hlen(&self, key: &[u8]) -> Result<usize, NativeRuntimeError> {
        NativeWriteBatch::hlen(self, key)
    }

    fn hscan(
        &self,
        key: &[u8],
        start_after: Option<&[u8]>,
        limit: usize,
    ) -> Result<HashEntries, NativeRuntimeError> {
        NativeWriteBatch::hscan(self, key, start_after, limit).map(entries)
    }

    fn hscan_reverse(
        &self,
        key: &[u8],
        start_before: Option<&[u8]>,
        limit: usize,
    ) -> Result<HashEntries, NativeRuntimeError> {
        NativeWriteBatch::hscan_reverse(self, key, start_before, limit).map(entries)
    }

    fn hscan_match(
        &self,
        key: &[u8],
        request: &HashPatternScanRequest,
    ) -> Result<PatternObservation, NativeRuntimeError> {
        NativeWriteBatch::hscan_match(self, key, request).map(|page| pattern_observation(&page))
    }
}

impl HashReadSurface for NativeSnapshot {
    fn label(&self) -> &'static str {
        "retained-or-materialized-snapshot"
    }

    fn hash_ttl(&self, key: &[u8]) -> Result<Ttl, NativeRuntimeError> {
        Ok(NativeSnapshot::ttl_hash(self, key))
    }

    fn field_ttl(&self, key: &[u8], field: &[u8]) -> Result<Ttl, NativeRuntimeError> {
        Ok(NativeSnapshot::ttl_hash_field(self, key, field))
    }

    fn hget(&self, key: &[u8], field: &[u8]) -> Result<Option<Vec<u8>>, NativeRuntimeError> {
        NativeSnapshot::hget(self, key, field).map(|value| value.map(<[u8]>::to_vec))
    }

    fn hlen(&self, key: &[u8]) -> Result<usize, NativeRuntimeError> {
        NativeSnapshot::hlen(self, key)
    }

    fn hscan(
        &self,
        key: &[u8],
        start_after: Option<&[u8]>,
        limit: usize,
    ) -> Result<HashEntries, NativeRuntimeError> {
        NativeSnapshot::hscan(self, key, start_after, limit).map(entries)
    }

    fn hscan_reverse(
        &self,
        key: &[u8],
        start_before: Option<&[u8]>,
        limit: usize,
    ) -> Result<HashEntries, NativeRuntimeError> {
        NativeSnapshot::hscan_reverse(self, key, start_before, limit).map(entries)
    }

    fn hscan_match(
        &self,
        key: &[u8],
        request: &HashPatternScanRequest,
    ) -> Result<PatternObservation, NativeRuntimeError> {
        NativeSnapshot::hscan_match(self, key, request).map(|page| pattern_observation(&page))
    }
}

struct PhysicalSurface<'database> {
    database: &'database NativeDatabase,
    logical_time_micros: i64,
}

impl HashReadSurface for PhysicalSurface<'_> {
    fn label(&self) -> &'static str {
        "current-root-physical"
    }

    fn hash_ttl(&self, key: &[u8]) -> Result<Ttl, NativeRuntimeError> {
        self.database.ttl_latest_hash(key, self.logical_time_micros)
    }

    fn field_ttl(&self, key: &[u8], field: &[u8]) -> Result<Ttl, NativeRuntimeError> {
        self.database
            .ttl_latest_hash_field(key, field, self.logical_time_micros)
    }

    fn hget(&self, key: &[u8], field: &[u8]) -> Result<Option<Vec<u8>>, NativeRuntimeError> {
        self.database
            .hget_latest_hash_at(key, field, self.logical_time_micros)
    }

    fn hlen(&self, key: &[u8]) -> Result<usize, NativeRuntimeError> {
        self.database
            .hlen_latest_hash_at(key, self.logical_time_micros)
    }

    fn hscan(
        &self,
        key: &[u8],
        start_after: Option<&[u8]>,
        limit: usize,
    ) -> Result<HashEntries, NativeRuntimeError> {
        self.database
            .hscan_latest_hash_at(key, start_after, limit, self.logical_time_micros)
            .map(entries)
    }

    fn hscan_reverse(
        &self,
        key: &[u8],
        start_before: Option<&[u8]>,
        limit: usize,
    ) -> Result<HashEntries, NativeRuntimeError> {
        self.database
            .hscan_reverse_latest_hash_at(key, start_before, limit, self.logical_time_micros)
            .map(entries)
    }

    fn hscan_match(
        &self,
        key: &[u8],
        request: &HashPatternScanRequest,
    ) -> Result<PatternObservation, NativeRuntimeError> {
        self.database
            .hscan_match_latest_hash_at(key, request, self.logical_time_micros)
            .map(|page| pattern_observation(&page))
    }
}

#[derive(Default)]
struct CorpusStats {
    actions: [usize; ACTION_KIND_COUNT],
    comparisons: usize,
    private_audits: usize,
    retained_audits: usize,
    materialized_audits: usize,
    physical_audits: usize,
    reopens: usize,
}

struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn index(&mut self, length: usize) -> usize {
        debug_assert!(length > 0);
        let bounded_length = u64::try_from(length).unwrap_or(u64::MAX);
        usize::try_from(self.next() % bounded_length).unwrap_or(0)
    }
}

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create(seed: u64) -> Result<Self, Box<dyn Error>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        Ok(Self(std::env::temp_dir().join(format!(
            "hyphae-hash-model-{}-{seed:016x}-{timestamp}",
            std::process::id()
        ))))
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

#[test]
fn fixed_hash_trace_matches_every_execution_surface() -> Result<(), Box<dyn Error>> {
    run_fixed_hash_model_corpus()
}

#[test]
fn perturbed_hash_oracle_reports_exact_trace_location() -> Result<(), Box<dyn Error>> {
    let temporary = TemporaryDirectory::create(0xfeed)?;
    let mut database = NativeDatabase::create(temporary.path())?;
    let key = hash_key(0);
    let field = hash_field(5);
    let mut transaction = database.begin(0, DurabilityClass::Memory)?;
    transaction.create_hash(key.clone())?;
    transaction.hset(key.clone(), field.clone(), b"native".to_vec())?;
    transaction.commit()?;

    let mut perturbed = StructureState::default();
    assert!(perturbed.create_hash(key));
    assert_eq!(
        perturbed.hset(&hash_key(0), field, b"perturbed".to_vec()),
        Some(true)
    );
    let action = Action::Probe {
        key: hash_key(0),
        field: hash_field(5),
    };
    let context = TraceContext {
        seed: 0xfeed,
        seed_ordinal: 3,
        step: 17,
        logical_time_micros: 0,
        action: &action,
    };
    let error = match audit_surface(
        &PhysicalSurface {
            database: &database,
            logical_time_micros: 0,
        },
        &perturbed,
        context,
        true,
    ) {
        Ok(comparisons) => {
            return Err(io::Error::other(format!(
                "perturbed oracle unexpectedly passed {comparisons} comparisons"
            ))
            .into());
        }
        Err(error) => error,
    };
    let message = error.to_string();
    assert!(message.contains("seed=0x000000000000feed"));
    assert!(message.contains("seed_ordinal=3"));
    assert!(message.contains("step=17"));
    assert!(message.contains("surface=current-root-physical"));
    Ok(())
}

fn run_fixed_hash_model_corpus() -> Result<(), Box<dyn Error>> {
    let mut stats = CorpusStats::default();
    for (seed_ordinal, seed) in FIXED_SEEDS.into_iter().enumerate() {
        run_seed(seed_ordinal, seed, &mut stats)?;
    }
    if stats.actions.contains(&0) {
        return Err(io::Error::other(format!(
            "fixed corpus did not reach every action kind: {:?}",
            stats.actions
        ))
        .into());
    }
    let expected_actions = FIXED_SEEDS.len() * STEPS_PER_SEED;
    if stats.actions.iter().sum::<usize>() != expected_actions {
        return Err(io::Error::other("fixed corpus action accounting diverged").into());
    }
    println!(
        "hash_model_corpus seeds={} steps_per_seed={} actions={} comparisons={} \
         private_audits={} retained_audits={} materialized_audits={} \
         physical_audits={} reopens={}",
        FIXED_SEEDS.len(),
        STEPS_PER_SEED,
        expected_actions,
        stats.comparisons,
        stats.private_audits,
        stats.retained_audits,
        stats.materialized_audits,
        stats.physical_audits,
        stats.reopens
    );
    Ok(())
}

fn run_seed(seed_ordinal: usize, seed: u64, stats: &mut CorpusStats) -> Result<(), Box<dyn Error>> {
    let temporary = TemporaryDirectory::create(seed)?;
    let mut database = NativeDatabase::create(temporary.path())?;
    let mut oracle = StructureState::default();
    let mut random = SplitMix64::new(seed);
    let mut logical_time_micros = 0_i64;

    for step in 0..STEPS_PER_SEED {
        let action = generate_action(&mut random, step, logical_time_micros);
        if let Action::AdvanceTime { target } = action {
            logical_time_micros = target;
        }
        let context = TraceContext {
            seed,
            seed_ordinal,
            step,
            logical_time_micros,
            action: &action,
        };
        stats.actions[action.kind_index()] += 1;
        let oracle_before = oracle.clone();
        let retained = database.snapshot(logical_time_micros)?;
        let mut candidate = oracle.clone();
        let mut transaction = database.begin(logical_time_micros, DurabilityClass::Memory)?;
        let expected = apply_model_action(&mut candidate, &action, logical_time_micros)?;
        let actual = apply_native_action(&mut transaction, &action)?;
        compare(context, "private", "command-outcome", &expected, &actual)?;
        stats.comparisons += 1;
        let deep_audit = step % 16 == 15;
        stats.comparisons += audit_surface(&*transaction, &candidate, context, deep_audit)?;
        stats.private_audits += 1;
        let has_mutations = !transaction.batch.mutations.is_empty();
        if has_mutations {
            transaction.commit()?;
        } else {
            transaction.rollback();
        }
        oracle = candidate;

        stats.comparisons += audit_surface(&retained, &oracle_before, context, deep_audit)?;
        stats.retained_audits += 1;
        let materialized = database.snapshot(logical_time_micros)?;
        stats.comparisons += audit_surface(&materialized, &oracle, context, deep_audit)?;
        stats.materialized_audits += 1;
        stats.comparisons += audit_surface(
            &PhysicalSurface {
                database: &database,
                logical_time_micros,
            },
            &oracle,
            context,
            deep_audit,
        )?;
        stats.physical_audits += 1;
        drop(materialized);
        drop(retained);

        if (step + 1) % REOPEN_INTERVAL == 0 {
            drop(database);
            database = NativeDatabase::open(temporary.path())?;
            stats.reopens += 1;
            stats.comparisons += audit_surface(
                &PhysicalSurface {
                    database: &database,
                    logical_time_micros,
                },
                &oracle,
                context,
                true,
            )?;
            stats.physical_audits += 1;
        }
    }
    drop(database);
    let reopened = NativeDatabase::open(temporary.path())?;
    let final_action = Action::Probe {
        key: hash_key(0),
        field: hash_field(0),
    };
    let final_context = TraceContext {
        seed,
        seed_ordinal,
        step: STEPS_PER_SEED,
        logical_time_micros,
        action: &final_action,
    };
    stats.comparisons += audit_surface(
        &PhysicalSurface {
            database: &reopened,
            logical_time_micros,
        },
        &oracle,
        final_context,
        true,
    )?;
    stats.physical_audits += 1;
    Ok(())
}

fn generate_action(random: &mut SplitMix64, step: usize, time: i64) -> Action {
    if let Some(action) = fixed_prelude(step) {
        return action;
    }
    let key = hash_key(random.index(HASH_KEY_COUNT));
    let field = hash_field(random.index(HASH_FIELD_COUNT));
    match random.index(100) {
        0..=7 => Action::CreateHash { key },
        8..=14 => Action::DeleteHash { key },
        15..=21 => Action::ExpireHash {
            key,
            expiry: random_expiry(random, time),
        },
        22..=40 => Action::Hset {
            key,
            field,
            value: random_value(random),
        },
        41..=50 => Action::HsetMany {
            key,
            updates: random_updates(random),
        },
        51..=59 => Action::Hdelete { key, field },
        60..=67 => Action::HdeleteMany {
            key,
            fields: random_fields(random),
        },
        68..=75 => Action::Hincrement {
            key,
            field,
            delta: random_delta(random),
        },
        76..=85 => Action::ExpireField {
            key,
            field,
            expiry: random_expiry(random, time),
        },
        86..=93 => Action::AdvanceTime {
            target: time.saturating_add(i64::try_from(random.index(5)).unwrap_or_default()),
        },
        _ => Action::Probe { key, field },
    }
}

fn fixed_prelude(step: usize) -> Option<Action> {
    let key = hash_key(0);
    let field = hash_field(5);
    match step {
        0 | 11 => Some(Action::CreateHash { key }),
        1 => Some(Action::Hset {
            key,
            field,
            value: b"1".to_vec(),
        }),
        2 => Some(Action::ExpireField {
            key,
            field,
            expiry: 5,
        }),
        3 => Some(Action::AdvanceTime { target: 4 }),
        4 | 9 => Some(Action::Probe { key, field }),
        5 => Some(Action::AdvanceTime { target: 5 }),
        6 => Some(Action::Hset {
            key,
            field,
            value: b"2".to_vec(),
        }),
        7 => Some(Action::ExpireHash { key, expiry: 10 }),
        8 => Some(Action::AdvanceTime { target: 9 }),
        10 => Some(Action::AdvanceTime { target: 10 }),
        12 => Some(Action::HsetMany {
            key,
            updates: vec![
                (hash_field(6), b"3".to_vec()),
                (hash_field(7), b"binary\0".to_vec()),
            ],
        }),
        13 => Some(Action::Hincrement {
            key,
            field: hash_field(6),
            delta: 4,
        }),
        14 => Some(Action::HdeleteMany {
            key,
            fields: vec![hash_field(6), hash_field(31)],
        }),
        15 => Some(Action::ExpireField {
            key,
            field: hash_field(7),
            expiry: 10,
        }),
        _ => None,
    }
}

fn random_expiry(random: &mut SplitMix64, time: i64) -> i64 {
    const OFFSETS: [i64; 5] = [-1, 0, 1, 2, 8];
    time.saturating_add(OFFSETS[random.index(OFFSETS.len())])
}

fn random_delta(random: &mut SplitMix64) -> i64 {
    const DELTAS: [i64; 7] = [-2, -1, 0, 1, 2, i64::MIN, i64::MAX];
    DELTAS[random.index(DELTAS.len())]
}

fn random_value(random: &mut SplitMix64) -> Vec<u8> {
    match random.index(9) {
        0 => b"0".to_vec(),
        1 => b"1".to_vec(),
        2 => b"-1".to_vec(),
        3 => i64::MAX.to_string().into_bytes(),
        4 => i64::MIN.to_string().into_bytes(),
        5 => b"01".to_vec(),
        6 => b"binary\0value".to_vec(),
        7 => vec![0xff, 0, 0x7f],
        _ => i64::from_ne_bytes(random.next().to_ne_bytes())
            .to_string()
            .into_bytes(),
    }
}

fn random_updates(random: &mut SplitMix64) -> Vec<(Vec<u8>, Vec<u8>)> {
    let requested = 1 + random.index(4);
    let mut updates = BTreeMap::new();
    while updates.len() < requested {
        updates.insert(
            hash_field(random.index(HASH_FIELD_COUNT)),
            random_value(random),
        );
    }
    updates.into_iter().collect()
}

fn random_fields(random: &mut SplitMix64) -> Vec<Vec<u8>> {
    let requested = 1 + random.index(4);
    let mut fields = BTreeSet::new();
    while fields.len() < requested {
        fields.insert(hash_field(random.index(HASH_FIELD_COUNT)));
    }
    fields.into_iter().collect()
}

fn hash_key(index: usize) -> Vec<u8> {
    match index {
        0 => b"tenant-a:hash".to_vec(),
        1 => b"tenant-b:\0hash".to_vec(),
        2 => vec![0xff, b'h', b'a', b's', b'h'],
        3 => b"z:hash".to_vec(),
        _ => unreachable!("fixed hash-key index"),
    }
}

fn hash_field(index: usize) -> Vec<u8> {
    match index {
        0 => Vec::new(),
        1 => vec![0],
        2 => vec![0xff],
        3 => b"a".to_vec(),
        4 => b"a\0".to_vec(),
        5 => b"tenant:00".to_vec(),
        6 => b"tenant:01".to_vec(),
        7 => b"tenant:tail".to_vec(),
        8 => b"x:tail".to_vec(),
        9 => b"prefix".to_vec(),
        10 => b"prefix\0".to_vec(),
        11 => b"prefix\xff".to_vec(),
        12..HASH_FIELD_COUNT => {
            let mut field = b"field:".to_vec();
            field.extend_from_slice(&u16::try_from(index).unwrap_or_default().to_be_bytes());
            field
        }
        _ => unreachable!("fixed hash-field index"),
    }
}

fn apply_model_action(
    oracle: &mut StructureState,
    action: &Action,
    time: i64,
) -> Result<CommandOutcome, Box<dyn Error>> {
    Ok(match action {
        Action::CreateHash { key } => {
            if oracle.hash_is_expired(key, time) {
                let removed = oracle.delete_hash(key);
                debug_assert!(removed);
            }
            if oracle.create_hash(key.clone()) {
                CommandOutcome::Unit
            } else {
                CommandOutcome::Error(ExpectedError::KeyExists)
            }
        }
        Action::DeleteHash { key } => {
            if oracle.hash_is_visible(key, time) {
                CommandOutcome::Bool(oracle.delete_hash(key))
            } else {
                CommandOutcome::Bool(false)
            }
        }
        Action::ExpireHash { key, expiry } => {
            CommandOutcome::Bool(oracle.expire_hash(key, *expiry, time))
        }
        Action::Hset { key, field, value } => {
            match oracle.hset_at(key, field.clone(), value.clone(), time) {
                Some(true) => CommandOutcome::HashSet(HashSetOutcome::Added),
                Some(false) => CommandOutcome::HashSet(HashSetOutcome::Updated),
                None => CommandOutcome::Error(ExpectedError::UnknownHash),
            }
        }
        Action::HsetMany { key, updates } => match oracle.hset_many_at(key, updates, time) {
            Some(added) => CommandOutcome::Count(added),
            None => CommandOutcome::Error(ExpectedError::UnknownHash),
        },
        Action::Hdelete { key, field } => {
            if !oracle.hash_is_visible(key, time) {
                CommandOutcome::Error(ExpectedError::UnknownHash)
            } else if oracle.hget_at(key, field, time).is_none() {
                CommandOutcome::Bool(false)
            } else {
                CommandOutcome::Bool(oracle.hdelete(key, field).unwrap_or(false))
            }
        }
        Action::HdeleteMany { key, fields } => {
            if oracle.hash_is_visible(key, time) {
                let live = fields
                    .iter()
                    .filter(|field| oracle.hget_at(key, field, time).is_some())
                    .cloned()
                    .collect::<Vec<_>>();
                CommandOutcome::Count(oracle.hdelete_many(key, &live).unwrap_or(0))
            } else {
                CommandOutcome::Error(ExpectedError::UnknownHash)
            }
        }
        Action::Hincrement { key, field, delta } => {
            match oracle.hincrement_i64_at(key, field, *delta, time) {
                Ok(Some(value)) => CommandOutcome::Integer(value),
                Ok(None) => CommandOutcome::Error(ExpectedError::UnknownHash),
                Err(error) => CommandOutcome::Error(model_error(error)?),
            }
        }
        Action::ExpireField { key, field, expiry } => CommandOutcome::Bool(
            oracle
                .expire_hash_field(key, field, *expiry, time)
                .unwrap_or(false),
        ),
        Action::AdvanceTime { .. } | Action::Probe { .. } => CommandOutcome::Noop,
    })
}

fn apply_native_action(
    transaction: &mut crate::NativeTransaction<'_>,
    action: &Action,
) -> Result<CommandOutcome, Box<dyn Error>> {
    let outcome = match action {
        Action::CreateHash { key } => transaction
            .create_hash(key.clone())
            .map(|()| CommandOutcome::Unit),
        Action::DeleteHash { key } => transaction
            .delete_hash(key.clone())
            .map(CommandOutcome::Bool),
        Action::ExpireHash { key, expiry } => transaction
            .expire_hash(key.clone(), *expiry)
            .map(CommandOutcome::Bool),
        Action::Hset { key, field, value } => transaction
            .hset(key.clone(), field.clone(), value.clone())
            .map(CommandOutcome::HashSet),
        Action::HsetMany { key, updates } => transaction
            .hset_many(key.clone(), updates.clone())
            .map(CommandOutcome::Count),
        Action::Hdelete { key, field } => transaction
            .hdelete(key.clone(), field.clone())
            .map(CommandOutcome::Bool),
        Action::HdeleteMany { key, fields } => transaction
            .hdelete_many(key.clone(), fields.clone())
            .map(CommandOutcome::Count),
        Action::Hincrement { key, field, delta } => transaction
            .hincrement_i64(key.clone(), field.clone(), *delta)
            .map(CommandOutcome::Integer),
        Action::ExpireField { key, field, expiry } => transaction
            .expire_hash_field(key.clone(), field.clone(), *expiry)
            .map(CommandOutcome::Bool),
        Action::AdvanceTime { .. } | Action::Probe { .. } => {
            return Ok(CommandOutcome::Noop);
        }
    };
    match outcome {
        Ok(outcome) => Ok(outcome),
        Err(error) => Ok(CommandOutcome::Error(native_error(error)?)),
    }
}

fn model_error(error: ModelError) -> Result<ExpectedError, Box<dyn Error>> {
    match error {
        ModelError::StructureValueNotInteger => Ok(ExpectedError::ValueNotInteger),
        ModelError::StructureIntegerOverflow => Ok(ExpectedError::IntegerOverflow),
        other => Err(io::Error::other(format!("unexpected model error: {other}")).into()),
    }
}

fn native_error(error: NativeRuntimeError) -> Result<ExpectedError, Box<dyn Error>> {
    match error {
        NativeRuntimeError::StructureKeyExists => Ok(ExpectedError::KeyExists),
        NativeRuntimeError::UnknownStructureHash => Ok(ExpectedError::UnknownHash),
        NativeRuntimeError::StructureValueNotInteger => Ok(ExpectedError::ValueNotInteger),
        NativeRuntimeError::StructureIntegerOverflow => Ok(ExpectedError::IntegerOverflow),
        other => Err(io::Error::other(format!("unexpected native error: {other}")).into()),
    }
}

fn audit_surface(
    surface: &impl HashReadSurface,
    oracle: &StructureState,
    context: TraceContext<'_>,
    deep: bool,
) -> Result<usize, Box<dyn Error>> {
    let mut comparisons = 0;
    for key_index in 0..HASH_KEY_COUNT {
        let key = hash_key(key_index);
        comparisons += audit_hash_key(surface, oracle, &key, context, deep)?;
    }
    Ok(comparisons)
}

fn audit_hash_key(
    surface: &impl HashReadSurface,
    oracle: &StructureState,
    key: &[u8],
    context: TraceContext<'_>,
    deep: bool,
) -> Result<usize, Box<dyn Error>> {
    let label = surface.label();
    let visible = oracle.hash_is_visible(key, context.logical_time_micros);
    let ttl_check = format!("ttl-hash key={}", hex(key));
    let actual_ttl = surface_value(context, label, &ttl_check, surface.hash_ttl(key))?;
    compare(
        context,
        label,
        &ttl_check,
        &oracle_hash_ttl(oracle, key, context.logical_time_micros),
        &actual_ttl,
    )?;

    let mut comparisons = 1;
    comparisons += audit_hash_fields(surface, oracle, key, context, visible)?;
    comparisons += audit_hash_collection(surface, oracle, key, context, visible)?;
    if visible && deep {
        comparisons += audit_cursors_and_patterns(surface, oracle, key, context)?;
    }
    Ok(comparisons)
}

fn audit_hash_fields(
    surface: &impl HashReadSurface,
    oracle: &StructureState,
    key: &[u8],
    context: TraceContext<'_>,
    visible: bool,
) -> Result<usize, Box<dyn Error>> {
    let label = surface.label();
    for field_index in 0..HASH_FIELD_COUNT {
        let field = hash_field(field_index);
        let ttl_check = format!("ttl-field key={} field={}", hex(key), hex(&field));
        let actual_ttl = surface_value(context, label, &ttl_check, surface.field_ttl(key, &field))?;
        compare(
            context,
            label,
            &ttl_check,
            &oracle_field_ttl(oracle, key, &field, context.logical_time_micros),
            &actual_ttl,
        )?;

        let value_check = format!("hget key={} field={}", hex(key), hex(&field));
        let actual_value = surface.hget(key, &field);
        if visible {
            let expected_value = oracle
                .hget_at(key, &field, context.logical_time_micros)
                .map(<[u8]>::to_vec);
            compare(
                context,
                label,
                &value_check,
                &expected_value,
                &surface_value(context, label, &value_check, actual_value)?,
            )?;
        } else {
            expect_unknown_hash(context, label, &value_check, &actual_value)?;
        }
    }
    Ok(HASH_FIELD_COUNT * 2)
}

fn audit_hash_collection(
    surface: &impl HashReadSurface,
    oracle: &StructureState,
    key: &[u8],
    context: TraceContext<'_>,
    visible: bool,
) -> Result<usize, Box<dyn Error>> {
    let label = surface.label();
    let length_check = format!("hlen key={}", hex(key));
    let actual_length = surface.hlen(key);
    let ascending_check = format!("hscan-complete key={}", hex(key));
    let actual_ascending = surface.hscan(key, None, COMPLETE_SCAN_LIMIT);
    let descending_check = format!("hscan-reverse-complete key={}", hex(key));
    let actual_descending = surface.hscan_reverse(key, None, COMPLETE_SCAN_LIMIT);
    if visible {
        let expected_length = model_value(
            context,
            label,
            &length_check,
            oracle.hlen_at(key, context.logical_time_micros),
        )?;
        compare(
            context,
            label,
            &length_check,
            &expected_length,
            &surface_value(context, label, &length_check, actual_length)?,
        )?;
        let expected_ascending = model_value(
            context,
            label,
            &ascending_check,
            oracle.hscan_at(key, None, COMPLETE_SCAN_LIMIT, context.logical_time_micros),
        )?;
        compare(
            context,
            label,
            &ascending_check,
            &expected_ascending,
            &surface_value(context, label, &ascending_check, actual_ascending)?,
        )?;
        let expected_descending = model_value(
            context,
            label,
            &descending_check,
            oracle.hscan_reverse_at(key, None, COMPLETE_SCAN_LIMIT, context.logical_time_micros),
        )?;
        compare(
            context,
            label,
            &descending_check,
            &expected_descending,
            &surface_value(context, label, &descending_check, actual_descending)?,
        )?;
    } else {
        expect_unknown_hash(context, label, &length_check, &actual_length)?;
        expect_unknown_hash(context, label, &ascending_check, &actual_ascending)?;
        expect_unknown_hash(context, label, &descending_check, &actual_descending)?;
    }
    Ok(3)
}

fn audit_cursors_and_patterns(
    surface: &impl HashReadSurface,
    oracle: &StructureState,
    key: &[u8],
    context: TraceContext<'_>,
) -> Result<usize, Box<dyn Error>> {
    let label = surface.label();
    let expected_ascending = model_value(
        context,
        label,
        "deep ascending model scan",
        oracle.hscan_at(key, None, COMPLETE_SCAN_LIMIT, context.logical_time_micros),
    )?;
    let expected_descending = model_value(
        context,
        label,
        "deep descending model scan",
        oracle.hscan_reverse_at(key, None, COMPLETE_SCAN_LIMIT, context.logical_time_micros),
    )?;
    audit_paginated_scans(
        surface,
        key,
        context,
        &expected_ascending,
        &expected_descending,
    )?;
    audit_exact_cursors(surface, oracle, key, context)?;
    audit_patterns(surface, key, context, &expected_ascending)?;
    Ok(13)
}

fn audit_paginated_scans(
    surface: &impl HashReadSurface,
    key: &[u8],
    context: TraceContext<'_>,
    expected_ascending: &HashEntries,
    expected_descending: &HashEntries,
) -> Result<(), Box<dyn Error>> {
    let label = surface.label();
    let page_size = 1 + ((context.step + key.len()) % 5);
    let ascending_check = format!("hscan-paginated key={} page_size={page_size}", hex(key));
    let actual_ascending = surface_value(
        context,
        label,
        &ascending_check,
        exhaust_ascending(surface, key, page_size),
    )?;
    compare(
        context,
        label,
        &ascending_check,
        expected_ascending,
        &actual_ascending,
    )?;

    let descending_check = format!(
        "hscan-reverse-paginated key={} page_size={page_size}",
        hex(key)
    );
    let actual_descending = surface_value(
        context,
        label,
        &descending_check,
        exhaust_descending(surface, key, page_size),
    )?;
    compare(
        context,
        label,
        &descending_check,
        expected_descending,
        &actual_descending,
    )
}

fn audit_exact_cursors(
    surface: &impl HashReadSurface,
    oracle: &StructureState,
    key: &[u8],
    context: TraceContext<'_>,
) -> Result<(), Box<dyn Error>> {
    let label = surface.label();
    let cursor = hash_field((context.step + key.len()) % HASH_FIELD_COUNT);
    let ascending_check = format!(
        "hscan-exact-cursor key={} cursor={}",
        hex(key),
        hex(&cursor)
    );
    let expected_after = model_value(
        context,
        label,
        &ascending_check,
        oracle.hscan_at(
            key,
            Some(&cursor),
            COMPLETE_SCAN_LIMIT,
            context.logical_time_micros,
        ),
    )?;
    let actual_after = surface_value(
        context,
        label,
        &ascending_check,
        surface.hscan(key, Some(&cursor), COMPLETE_SCAN_LIMIT),
    )?;
    compare(
        context,
        label,
        &ascending_check,
        &expected_after,
        &actual_after,
    )?;

    let descending_check = format!(
        "hscan-reverse-exact-cursor key={} cursor={}",
        hex(key),
        hex(&cursor)
    );
    let expected_before = model_value(
        context,
        label,
        &descending_check,
        oracle.hscan_reverse_at(
            key,
            Some(&cursor),
            COMPLETE_SCAN_LIMIT,
            context.logical_time_micros,
        ),
    )?;
    let actual_before = surface_value(
        context,
        label,
        &descending_check,
        surface.hscan_reverse(key, Some(&cursor), COMPLETE_SCAN_LIMIT),
    )?;
    compare(
        context,
        label,
        &descending_check,
        &expected_before,
        &actual_before,
    )
}

fn audit_patterns(
    surface: &impl HashReadSurface,
    key: &[u8],
    context: TraceContext<'_>,
    expected_ascending: &HashEntries,
) -> Result<(), Box<dyn Error>> {
    let label = surface.label();
    for (pattern, predicate) in [
        (
            b"tenant:00".as_slice(),
            PatternPredicate::Exact(b"tenant:00"),
        ),
        (b"tenant:*".as_slice(), PatternPredicate::Prefix(b"tenant:")),
        (b"*tail".as_slice(), PatternPredicate::Suffix(b"tail")),
    ] {
        let request = HashPatternScanRequest::try_new(
            pattern.to_vec(),
            None,
            COMPLETE_SCAN_LIMIT,
            COMPLETE_SCAN_LIMIT,
            PATTERN_MATCH_STEP_LIMIT,
        )?;
        let match_check = format!("hscan-match key={} pattern={}", hex(key), hex(pattern));
        let actual = surface_value(
            context,
            label,
            &match_check,
            surface.hscan_match(key, &request),
        )?;
        let expected = expected_ascending
            .iter()
            .filter(|(field, _)| predicate.matches(field))
            .cloned()
            .collect::<Vec<_>>();
        compare(context, label, &match_check, &expected, &actual.entries)?;
        compare(
            context,
            label,
            &format!("hscan-match-stop key={} pattern={}", hex(key), hex(pattern)),
            &HashPatternScanStop::Exhausted,
            &actual.stop,
        )?;
        compare(
            context,
            label,
            &format!(
                "hscan-match-continuation key={} pattern={}",
                hex(key),
                hex(pattern)
            ),
            &None::<Vec<u8>>,
            &actual.continuation,
        )?;
        if actual.visited > COMPLETE_SCAN_LIMIT || actual.match_steps > PATTERN_MATCH_STEP_LIMIT {
            return Err(failure(
                context,
                label,
                &format!(
                    "hscan-match-bounds key={} pattern={} visited={} steps={}",
                    hex(key),
                    hex(pattern),
                    actual.visited,
                    actual.match_steps
                ),
            ));
        }
    }
    Ok(())
}

fn exhaust_ascending(
    surface: &impl HashReadSurface,
    key: &[u8],
    page_size: usize,
) -> Result<HashEntries, NativeRuntimeError> {
    let mut output = Vec::new();
    let mut cursor = None;
    for _ in 0..=HASH_FIELD_COUNT {
        let page = surface.hscan(key, cursor.as_deref(), page_size)?;
        if page.is_empty() {
            return Ok(output);
        }
        cursor = page.last().map(|(field, _)| field.clone());
        output.extend(page);
    }
    Err(NativeRuntimeError::InvalidStructureTree)
}

fn exhaust_descending(
    surface: &impl HashReadSurface,
    key: &[u8],
    page_size: usize,
) -> Result<HashEntries, NativeRuntimeError> {
    let mut output = Vec::new();
    let mut cursor = None;
    for _ in 0..=HASH_FIELD_COUNT {
        let page = surface.hscan_reverse(key, cursor.as_deref(), page_size)?;
        if page.is_empty() {
            return Ok(output);
        }
        cursor = page.last().map(|(field, _)| field.clone());
        output.extend(page);
    }
    Err(NativeRuntimeError::InvalidStructureTree)
}

#[derive(Clone, Copy)]
enum PatternPredicate {
    Exact(&'static [u8]),
    Prefix(&'static [u8]),
    Suffix(&'static [u8]),
}

impl PatternPredicate {
    fn matches(self, field: &[u8]) -> bool {
        match self {
            Self::Exact(value) => field == value,
            Self::Prefix(value) => field.starts_with(value),
            Self::Suffix(value) => field.ends_with(value),
        }
    }
}

fn oracle_hash_ttl(oracle: &StructureState, key: &[u8], time: i64) -> Ttl {
    match oracle.ttl_hash_micros(key, time) {
        None => Ttl::Missing,
        Some(TtlValue::Persistent) => Ttl::Persistent,
        Some(TtlValue::Remaining(value)) => Ttl::RemainingMicros(value),
    }
}

fn oracle_field_ttl(oracle: &StructureState, key: &[u8], field: &[u8], time: i64) -> Ttl {
    match oracle.ttl_hash_field_micros(key, field, time) {
        None => Ttl::Missing,
        Some(TtlValue::Persistent) => Ttl::Persistent,
        Some(TtlValue::Remaining(value)) => Ttl::RemainingMicros(value),
    }
}

fn expect_unknown_hash<T>(
    context: TraceContext<'_>,
    surface: &str,
    check: &str,
    actual: &Result<T, NativeRuntimeError>,
) -> Result<(), Box<dyn Error>> {
    match actual {
        Err(NativeRuntimeError::UnknownStructureHash) => Ok(()),
        Ok(_) => Err(failure(
            context,
            surface,
            &format!("{check} expected=UnknownStructureHash"),
        )),
        Err(error) => Err(failure(
            context,
            surface,
            &format!("{check} expected=UnknownStructureHash actual={error:?}"),
        )),
    }
}

fn model_value<T>(
    context: TraceContext<'_>,
    surface: &str,
    check: &str,
    value: Option<T>,
) -> Result<T, Box<dyn Error>> {
    value.ok_or_else(|| {
        failure(
            context,
            surface,
            &format!("{check} unexpected_missing_model_value"),
        )
    })
}

fn surface_value<T>(
    context: TraceContext<'_>,
    surface: &str,
    check: &str,
    result: Result<T, NativeRuntimeError>,
) -> Result<T, Box<dyn Error>> {
    result.map_err(|error| {
        failure(
            context,
            surface,
            &format!("{check} unexpected_error={error:?}"),
        )
    })
}

fn compare<T: fmt::Debug + PartialEq>(
    context: TraceContext<'_>,
    surface: &str,
    check: &str,
    expected: &T,
    actual: &T,
) -> Result<(), Box<dyn Error>> {
    if expected == actual {
        Ok(())
    } else {
        Err(failure(
            context,
            surface,
            &format!("{check} expected={expected:?} actual={actual:?}"),
        ))
    }
}

fn failure(context: TraceContext<'_>, surface: &str, check: &str) -> Box<dyn Error> {
    io::Error::other(format!(
        "hash model divergence {context} surface={surface} check={check}"
    ))
    .into()
}

fn entries(entries: Vec<HashFieldEntry>) -> HashEntries {
    entries
        .into_iter()
        .map(|entry| (entry.field().to_vec(), entry.value().to_vec()))
        .collect()
}

fn pattern_observation(page: &HashPatternScanPage) -> PatternObservation {
    PatternObservation {
        entries: entries(page.entries().to_vec()),
        continuation: page.continuation().map(<[u8]>::to_vec),
        stop: page.stop(),
        visited: page.visited(),
        match_steps: page.match_steps(),
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use fmt::Write as _;
        if write!(&mut output, "{byte:02x}").is_err() {
            return String::new();
        }
    }
    output
}
