//! The scripts of a transaction checked by Bitcoin Core's script interpreter
//! (libbitcoinconsensus, C++), the function [`ecsn::node::Config`] takes to check the scripts of
//! blocks and of the mempool's transactions. `no_std`: on a microcontroller it needs a C++
//! runtime and `malloc`, and its libsecp256k1 (or the firmware's, with `external-secp`).
//!
//! [`ecsn::node::Config`]: https://docs.rs/ecsn

#![no_std]

extern crate alloc;

use alloc::vec::Vec;

use bitcoin::TxOut;
use bitcoinconsensus::{verify_transaction, TxError, Utxo};

/// Checks the script of every input of `tx` (serialized), which spends `spent` in their order,
/// under Bitcoin Core's script verification `flags`. Returns the first input whose script fails;
/// a transaction that does not deserialize, or `spent` of another length, fails at input 0.
pub fn verify_scripts(tx: &[u8], spent: &[TxOut], flags: u32) -> Result<(), usize> {
    // every output spent, which Taproot's signatures commit to
    let utxos: Vec<Utxo> = spent
        .iter()
        .map(|out| Utxo {
            script_pubkey: out.script_pubkey.as_bytes().as_ptr(),
            script_pubkey_len: out.script_pubkey.len() as u32,
            value: out.value.to_sat() as i64,
        })
        .collect();
    verify_transaction(tx, &utxos, flags).map_err(|e| match e {
        TxError::Input(index) => index,
        TxError::Unusable(_) => 0,
    })
}
