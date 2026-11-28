//! The scripts: the flags of Bitcoin Core's script interpreter for a block, and the check of a
//! transaction's scripts by the function the node is given ([`VerifyScripts`]), which brings
//! the interpreter (`ecsn-consensus`: Bitcoin Core's libbitcoinconsensus).

use alloc::vec::Vec;

use bitcoin::consensus::encode::serialize;
use bitcoin::{BlockHash, Transaction, TxOut};
use floresta_chain::ChainParams;

/// Checks the script of every input of a transaction (serialized), which spends the outputs
/// given in their order, under Bitcoin Core's script verification flags. Returns the first input
/// whose script fails.
pub type VerifyScripts = fn(tx: &[u8], spent: &[TxOut], flags: u32) -> Result<(), usize>;

/// Bitcoin Core's script verification flags, those of the consensus rules.
pub mod flags {
    pub const NONE: u32 = 0;
    pub const P2SH: u32 = 1 << 0;
    pub const DERSIG: u32 = 1 << 2;
    pub const NULLDUMMY: u32 = 1 << 4;
    pub const CHECKLOCKTIMEVERIFY: u32 = 1 << 9;
    pub const CHECKSEQUENCEVERIFY: u32 = 1 << 10;
    pub const WITNESS: u32 = 1 << 11;
    pub const TAPROOT: u32 = 1 << 17;
}

/// When each script rule applies on a network.
#[derive(Clone, Debug)]
pub struct ScriptRules {
    bip65: u32,
    bip66: u32,
    csv: u32,
    segwit: u32,
    /// The blocks that break a rule that applies everywhere else, with their own flags.
    exceptions: Vec<(BlockHash, u32)>,
}

impl ScriptRules {
    pub fn new(params: &ChainParams) -> Self {
        Self {
            bip65: params.params.bip65_height,
            bip66: params.params.bip66_height,
            csv: params.csv_activation_height,
            segwit: params.segwit_activation_height,
            exceptions: params.exceptions.iter().map(|(h, f)| (*h, *f)).collect(),
        }
    }

    /// The flags of the block at `height`, `hash` if it is known (a block to come has none).
    /// P2SH, segwit and Taproot apply from the genesis block but in the two blocks that break
    /// them (on mainnet), as Bitcoin Core does.
    pub fn flags(&self, height: u32, hash: Option<&BlockHash>) -> u32 {
        if let Some(hash) = hash {
            if let Some((_, f)) = self.exceptions.iter().find(|(h, _)| h == hash) {
                return *f;
            }
        }
        let mut f = flags::P2SH | flags::WITNESS | flags::TAPROOT;
        if height >= self.bip65 {
            f |= flags::CHECKLOCKTIMEVERIFY;
        }
        if height >= self.bip66 {
            f |= flags::DERSIG;
        }
        if height >= self.csv {
            f |= flags::CHECKSEQUENCEVERIFY;
        }
        if height >= self.segwit {
            f |= flags::NULLDUMMY;
        }
        f
    }
}

/// Checks the scripts of `tx`, which spends `spent` in the order of its inputs, under `flags`.
/// Returns the first input whose script fails.
pub fn check(
    verify: VerifyScripts,
    tx: &Transaction,
    spent: &[TxOut],
    flags: u32,
) -> Result<(), usize> {
    verify(&serialize(tx), spent, flags)
}

#[cfg(test)]
mod tests {
    use bitcoin::Network;

    use super::*;

    #[test]
    fn mainnet_flags_follow_the_soft_forks() {
        let rules = ScriptRules::new(&ChainParams::from(Network::Bitcoin));
        // CHECKLOCKTIMEVERIFY at 388,381, DERSIG at 363,725, CSV at 419,328, segwit at 481,824
        assert_eq!(
            rules.flags(300_000, None),
            flags::P2SH | flags::WITNESS | flags::TAPROOT
        );
        let all = flags::P2SH
            | flags::DERSIG
            | flags::NULLDUMMY
            | flags::CHECKLOCKTIMEVERIFY
            | flags::CHECKSEQUENCEVERIFY
            | flags::WITNESS
            | flags::TAPROOT;
        assert_eq!(rules.flags(976_679, None), all);
        // the block that broke Taproot's rules
        let hash: BlockHash = "0000000000000000000f14c35b2d841e986ab5441de8c585d5ffe55ea1e395ad"
            .parse()
            .unwrap();
        assert_eq!(
            rules.flags(692_261, Some(&hash)),
            flags::P2SH | flags::WITNESS
        );
    }
}
