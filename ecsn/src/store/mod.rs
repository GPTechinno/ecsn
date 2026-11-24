//! [`ChainStore`](floresta_chain::ChainStore)s: in RAM, or on NOR flash.

mod flash;
mod mem;

pub use flash::{FlashChainStore, Layout, MAX_RECORDS, RECORD_SIZE};
pub use mem::MemChainStore;
