// SPDX-License-Identifier: Apache-2.0

//! Cross-crate consumption of the canonical primitive scalar corpus.

use hyphae_native_types::primitive_scalar_golden_fixtures;

#[test]
fn records_consumes_native_scalar_golden_bytes() -> Result<(), Box<dyn std::error::Error>> {
    for fixture in primitive_scalar_golden_fixtures()? {
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
