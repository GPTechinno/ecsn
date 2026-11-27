//! Embedded Compact State Node: a Utreexo compact state node over `floresta-chain`, `no_std`
//! with `alloc`, talking to its peers over any `embedded-io-async` byte stream.
//!
//! - [`p2p`]: the Bitcoin P2P messages the node uses, with BIP 183's `getuproof` and `uproof`,
//!   and their framing.
//! - [`addrbook`]: the peers the node knows of, and the DNS seeds.
//! - [`store`]: [`ChainStore`](floresta_chain::ChainStore)s in RAM, and on NOR [`flash`].
//! - [`node`]: the connections to a bridge and to ordinary peers, the header sync and the
//!   validation of each block with its Utreexo proof.
//! - [`sntp`]: the time of day from an SNTP server, for a board with no clock.

#![no_std]

extern crate alloc;

pub mod addrbook;
pub mod flash;
pub mod node;
pub mod p2p;
pub mod sntp;
pub mod store;
