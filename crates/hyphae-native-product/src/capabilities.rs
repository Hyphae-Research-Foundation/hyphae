// SPDX-License-Identifier: Apache-2.0

//! Stable capability and hard-limit discovery for product callers.

/// Current bounded embedded-product capabilities.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductCapabilities {
    /// Product contract version.
    pub product_api_version: u16,
    /// Native directory format version.
    pub native_directory_format: u16,
    /// Logical catalog object codec version (`HYCOBJ02`).
    pub logical_catalog_codec_version: u16,
    /// Persisted logical catalog tree version (`HYCAT006`).
    pub catalog_tree_format_version: u16,
    /// Maximum summaries or edges returned by one catalog request.
    pub max_catalog_items: usize,
    /// Maximum physical catalog entries visited by one catalog request.
    pub max_catalog_visits: usize,
    /// Maximum canonical catalog output bytes returned by one request.
    pub max_catalog_bytes: usize,
    /// Maximum UTF-8 SQL statement bytes.
    pub max_sql_statement_bytes: usize,
    /// Maximum SQL parameters.
    pub max_sql_parameters: usize,
    /// Maximum materialized SQL rows.
    pub max_sql_rows: usize,
}

/// Returns the immutable capability record for this build.
pub const fn capabilities() -> ProductCapabilities {
    ProductCapabilities {
        product_api_version: 1,
        native_directory_format: 1,
        logical_catalog_codec_version: 2,
        catalog_tree_format_version: 6,
        max_catalog_items: hyphae_native_runtime::MAX_CATALOG_READ_ITEMS,
        max_catalog_visits: hyphae_native_runtime::MAX_CATALOG_READ_VISITS,
        max_catalog_bytes: hyphae_native_runtime::MAX_CATALOG_READ_BYTES,
        max_sql_statement_bytes: crate::MAX_PRODUCT_SQL_STATEMENT_BYTES,
        max_sql_parameters: crate::MAX_PRODUCT_SQL_PARAMETERS,
        max_sql_rows: crate::MAX_PRODUCT_SQL_ROWS,
    }
}
