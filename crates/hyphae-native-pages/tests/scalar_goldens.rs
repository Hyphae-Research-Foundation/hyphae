// SPDX-License-Identifier: Apache-2.0

//! Cross-crate page-layer consumption of canonical primitive scalar bytes.

use hyphae_native_types::primitive_scalar_golden_fixtures;

#[test]
fn pages_consumes_native_scalar_golden_bytes() -> Result<(), Box<dyn std::error::Error>> {
    let fixtures = primitive_scalar_golden_fixtures()?;
    assert_eq!(fixtures.len(), 13);
    for fixture in fixtures {
        assert_eq!(
            fixture.value.encode_storage(&fixture.logical_type)?,
            fixture.storage
        );
        assert_eq!(
            fixture
                .value
                .encode_ordered_component(&fixture.logical_type)?,
            fixture.ordered
        );
    }
    Ok(())
}
