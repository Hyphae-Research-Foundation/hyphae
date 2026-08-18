// SPDX-License-Identifier: Apache-2.0

//! Portable `HYPHLCL1` framing and canonical Native product wire contracts.

#![forbid(unsafe_code)]
#![allow(clippy::missing_errors_doc)]

mod control;
mod frame;
mod handshake;
mod product;

pub use control::*;
pub use frame::*;
pub use handshake::*;
pub use product::*;
