// SPDX-License-Identifier: GPL-3.0-only

//! Cross-crate page-layer consumption of canonical primitive scalar bytes.

use hyphae_native_types::canonical_value_golden_fixtures;

#[test]
fn pages_consumes_native_scalar_golden_bytes() -> Result<(), Box<dyn std::error::Error>> {
    let fixtures = canonical_value_golden_fixtures()?;
    assert_eq!(fixtures.len(), 17);
    for fixture in fixtures {
        assert_eq!(
            fixture.value.encode_storage(&fixture.logical_type)?,
            fixture.storage
        );
        if !fixture.ordered.is_empty() {
            assert_eq!(
                fixture
                    .value
                    .encode_ordered_component(&fixture.logical_type)?,
                fixture.ordered
            );
        }
    }
    Ok(())
}
