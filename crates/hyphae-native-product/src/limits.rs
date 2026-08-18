// SPDX-License-Identifier: Apache-2.0

//! Central request count, byte, work, and memory limits.

use crate::{ProductError, ProductErrorCode};

/// Hard maximum count accepted by a request context.
pub const MAX_PRODUCT_CONTEXT_COUNT: usize = 4_096;
/// Hard maximum request or response bytes accepted by a request context.
pub const MAX_PRODUCT_CONTEXT_BYTES: usize = 16 * 1024 * 1024;
/// Hard maximum logical work units accepted by a request context.
pub const MAX_PRODUCT_CONTEXT_WORK_UNITS: usize = 1_000_000;
/// Hard maximum retained request and response memory.
pub const MAX_PRODUCT_CONTEXT_MEMORY_BYTES: usize = 64 * 1024 * 1024;
/// Hard maximum operations retained by one atomic or explicit transaction.
pub const MAX_PRODUCT_TRANSACTION_OPERATIONS: usize = 1_024;
/// Hard maximum fields retained by one stream append operation.
pub const MAX_PRODUCT_STREAM_FIELDS: usize = 4_096;

/// Complete caller-selected resource envelope for one product operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(clippy::struct_field_names)]
pub struct ProductLimits {
    /// Maximum independently supplied or returned items.
    pub max_count: usize,
    /// Maximum aggregate caller-controlled request bytes.
    pub max_request_bytes: usize,
    /// Maximum aggregate response bytes.
    pub max_response_bytes: usize,
    /// Maximum auditable logical work units.
    pub max_work_units: usize,
    /// Maximum request and result memory retained by the product layer.
    pub max_memory_bytes: usize,
}

impl Default for ProductLimits {
    fn default() -> Self {
        Self {
            max_count: MAX_PRODUCT_CONTEXT_COUNT,
            max_request_bytes: MAX_PRODUCT_CONTEXT_BYTES,
            max_response_bytes: MAX_PRODUCT_CONTEXT_BYTES,
            max_work_units: MAX_PRODUCT_CONTEXT_WORK_UNITS,
            max_memory_bytes: MAX_PRODUCT_CONTEXT_MEMORY_BYTES,
        }
    }
}

impl ProductLimits {
    /// Validates that every central bound is nonzero.
    ///
    /// # Errors
    ///
    /// Returns `limit_exceeded` if any bound is zero.
    pub fn validate(self) -> Result<(), ProductError> {
        if self.max_count == 0
            || self.max_request_bytes == 0
            || self.max_response_bytes == 0
            || self.max_work_units == 0
            || self.max_memory_bytes == 0
            || self.max_count > MAX_PRODUCT_CONTEXT_COUNT
            || self.max_request_bytes > MAX_PRODUCT_CONTEXT_BYTES
            || self.max_response_bytes > MAX_PRODUCT_CONTEXT_BYTES
            || self.max_work_units > MAX_PRODUCT_CONTEXT_WORK_UNITS
            || self.max_memory_bytes > MAX_PRODUCT_CONTEXT_MEMORY_BYTES
        {
            Err(ProductError::from_code(ProductErrorCode::LimitExceeded))
        } else {
            Ok(())
        }
    }

    pub(crate) fn admit_request(
        self,
        count: usize,
        bytes: usize,
        work: usize,
        memory: usize,
    ) -> Result<(), ProductError> {
        self.validate()?;
        if count > self.max_count
            || bytes > self.max_request_bytes
            || work > self.max_work_units
            || memory > self.max_memory_bytes
        {
            Err(ProductError::from_code(ProductErrorCode::LimitExceeded))
        } else {
            Ok(())
        }
    }

    pub(crate) fn admit_response(
        self,
        count: usize,
        bytes: usize,
        memory: usize,
    ) -> Result<(), ProductError> {
        if count > self.max_count
            || bytes > self.max_response_bytes
            || memory > self.max_memory_bytes
        {
            Err(ProductError::from_code(ProductErrorCode::LimitExceeded))
        } else {
            Ok(())
        }
    }
}
