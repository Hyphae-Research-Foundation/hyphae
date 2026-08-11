// SPDX-License-Identifier: AGPL-3.0-only

//! Process-local CLI client over the native product dispatcher.

use hyphae_native_product::{
    NativeProduct, ProductAuthorization, ProductDurability, ProductError, ProductOperation,
    ProductPrincipal, ProductResponse, ProductSession, ProductSessionId,
};
use uuid::Uuid;

use crate::native::logical_time_micros;

/// One direct embedded client. It never opens a transport listener.
pub(crate) struct EmbeddedClient {
    product: NativeProduct,
    session: ProductSession,
    next_request_id: u128,
}

impl EmbeddedClient {
    pub(crate) fn new(product: NativeProduct) -> Result<Self, Box<ProductError>> {
        let session_id = ProductSessionId::new(Uuid::now_v7().as_u128()).ok_or_else(|| {
            Box::new(ProductError::from_code(
                hyphae_native_product::ProductErrorCode::Internal,
            ))
        })?;
        let principal = ProductPrincipal::new("local:cli").ok_or_else(|| {
            Box::new(ProductError::from_code(
                hyphae_native_product::ProductErrorCode::Internal,
            ))
        })?;
        Ok(Self {
            product,
            session: ProductSession::new(session_id, principal, ProductAuthorization::ALL),
            next_request_id: 1,
        })
    }

    pub(crate) fn dispatch(
        &mut self,
        operation: ProductOperation,
    ) -> Result<ProductResponse, Box<ProductError>> {
        self.dispatch_with_durability(operation, ProductDurability::Strict)
    }

    pub(crate) fn dispatch_with_durability(
        &mut self,
        operation: ProductOperation,
        durability: ProductDurability,
    ) -> Result<ProductResponse, Box<ProductError>> {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.checked_add(1).ok_or_else(|| {
            Box::new(ProductError::from_code(
                hyphae_native_product::ProductErrorCode::LimitExceeded,
            ))
        })?;
        let mut context = hyphae_native_product::ProductRequestContext::new(
            request_id,
            self.session.id(),
            logical_time_micros(),
            self.session.principal().clone(),
            ProductAuthorization::ALL,
        );
        context.durability.durability = durability;
        self.product
            .dispatch(&mut self.session, &context, operation)
            .map_err(Box::new)
    }

    pub(crate) fn product_mut(&mut self) -> &mut NativeProduct {
        &mut self.product
    }
}
