//! A [`ChainStore`] in RAM: every header, the height index and the accumulator of each block.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use bitcoin::BlockHash;
use floresta_chain::{BestChain, ChainStore, ChainstoreError, DiskBlockHeader};

/// Bytes a header takes here: the header, its state, its key and its index entry.
const HEADER_COST: u64 = 80 + 8 + 32 + 32;

#[derive(Default)]
pub struct MemChainStore {
    headers: BTreeMap<BlockHash, DiskBlockHeader>,
    /// Block hash by height on the best chain.
    index: Vec<BlockHash>,
    /// Serialized accumulator after each block.
    roots: BTreeMap<u32, Vec<u8>>,
    best: Option<BestChain>,
}

impl MemChainStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ChainStore for MemChainStore {
    type Error = ChainstoreError;

    fn save_roots_for_block(&mut self, roots: Vec<u8>, height: u32) -> Result<(), Self::Error> {
        self.roots.insert(height, roots);
        Ok(())
    }

    fn load_roots_for_block(&mut self, height: u32) -> Result<Option<Vec<u8>>, Self::Error> {
        Ok(self.roots.get(&height).cloned())
    }

    fn load_height(&self) -> Result<Option<BestChain>, Self::Error> {
        Ok(self.best.clone())
    }

    fn save_height(&mut self, height: &BestChain) -> Result<(), Self::Error> {
        self.best = Some(height.clone());
        Ok(())
    }

    fn get_header(&self, block_hash: &BlockHash) -> Result<Option<DiskBlockHeader>, Self::Error> {
        Ok(self.headers.get(block_hash).copied())
    }

    fn get_header_by_height(&self, height: u32) -> Result<Option<DiskBlockHeader>, Self::Error> {
        Ok(self
            .index
            .get(height as usize)
            .and_then(|hash| self.headers.get(hash))
            .copied())
    }

    fn save_header(&mut self, header: &DiskBlockHeader) -> Result<(), Self::Error> {
        self.headers.insert(header.block_hash(), *header);
        Ok(())
    }

    fn get_block_hash(&self, height: u32) -> Result<Option<BlockHash>, Self::Error> {
        Ok(self.index.get(height as usize).copied())
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn update_block_index(&mut self, height: u32, hash: BlockHash) -> Result<(), Self::Error> {
        let height = height as usize;
        match height.cmp(&self.index.len()) {
            core::cmp::Ordering::Less => self.index[height] = hash,
            core::cmp::Ordering::Equal => self.index.push(hash),
            core::cmp::Ordering::Greater => {
                return Err(ChainstoreError::Other(String::from(
                    "block index entry past the end of the index",
                )))
            }
        }
        Ok(())
    }

    fn check_integrity(&self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn size_on_disk(&self) -> Result<u64, Self::Error> {
        let roots: usize = self.roots.values().map(Vec::len).sum();
        Ok(self.headers.len() as u64 * HEADER_COST + roots as u64)
    }
}
