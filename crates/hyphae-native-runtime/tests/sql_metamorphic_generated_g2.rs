// SPDX-License-Identifier: AGPL-3.0-only

//! Seeded generated metamorphic SQL equivalence checks.

use hyphae_native_runtime::{NativeDatabase, SqlResult};
use hyphae_native_types::DurabilityClass;

fn next(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

#[test]
fn seeded_boolean_and_range_rewrites_hold_for_256_cases() -> Result<(), Box<dyn std::error::Error>>
{
    let temporary = std::env::temp_dir().join(format!(
        "hyphae-native-metamorphic-generated-{}",
        std::process::id()
    ));
    let _ignored = std::fs::remove_dir_all(&temporary);
    let mut database = NativeDatabase::create(&temporary)?;
    let mut transaction = database.begin_sql(1, DurabilityClass::Memory)?;
    transaction.execute_sql(
        "CREATE TABLE events (id BIGINT PRIMARY KEY, tenant TEXT NOT NULL, active BOOLEAN NOT NULL)",
        &[],
    )?;
    for id in 0..64 {
        let tenant = if id % 3 == 0 {
            "a"
        } else if id % 3 == 1 {
            "b"
        } else {
            "c"
        };
        let active = if id % 2 == 0 { "TRUE" } else { "FALSE" };
        transaction.execute_sql(
            &format!("INSERT INTO events (id, tenant, active) VALUES ({id}, '{tenant}', {active})"),
            &[],
        )?;
    }

    let mut seed = 20_260_804_u64;
    for case in 0..256 {
        let first = next(&mut seed) % 64;
        let second = next(&mut seed) % 64;
        let lower = first.min(second);
        let upper = first.max(second);
        let tenant = match next(&mut seed) % 3 {
            0 => "a",
            1 => "b",
            _ => "c",
        };
        let active = next(&mut seed) & 1 == 0;
        let active_sql = if active { "TRUE" } else { "FALSE" };
        let pairs = [
            (
                format!(
                    "SELECT id FROM events WHERE tenant = '{tenant}' AND active = {active_sql} ORDER BY id LIMIT 64"
                ),
                format!(
                    "SELECT id FROM events WHERE active = {active_sql} AND tenant = '{tenant}' ORDER BY id LIMIT 64"
                ),
            ),
            (
                format!(
                    "SELECT id FROM events WHERE id >= {lower} AND id <= {upper} ORDER BY id LIMIT 64"
                ),
                format!(
                    "SELECT id FROM events WHERE id <= {upper} AND id >= {lower} ORDER BY id LIMIT 64"
                ),
            ),
            (
                format!("SELECT id FROM events WHERE NOT NOT id >= {lower} ORDER BY id LIMIT 64"),
                format!("SELECT id FROM events WHERE id >= {lower} ORDER BY id LIMIT 64"),
            ),
        ];
        for (left, right) in pairs {
            let left_result = transaction.execute_sql(&left, &[])?;
            let right_result = transaction.execute_sql(&right, &[])?;
            assert_eq!(left_result, right_result, "case {case}: {left} <> {right}");
            assert!(matches!(left_result, SqlResult::Rows { .. }));
        }
    }
    transaction.rollback();
    std::fs::remove_dir_all(&temporary)?;
    Ok(())
}
