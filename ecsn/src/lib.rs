//! Embedded Compact State Node: a Utreexo compact state node over `floresta-chain`, `no_std`
//! with `alloc`, talking to its peers over any `embedded-io-async` byte stream.
//!
//! - [`p2p`]: the Bitcoin P2P messages the node uses, with BIP 183's `getuproof` and `uproof`,
//!   and their framing.
//! - [`store`]: [`ChainStore`](floresta_chain::ChainStore)s in RAM, and on NOR [`flash`].
//! - [`node`]: the handshake, the header sync and the validation of each block with its
//!   Utreexo proof.

#![no_std]

extern crate alloc;

pub mod flash;
pub mod node;
pub mod p2p;
pub mod store;
