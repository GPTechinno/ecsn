//! NOR flash, as the [flash chain store](crate::store::FlashChainStore) uses it: reads anywhere,
//! programs that can only clear bits, erases that set whole blocks back to `0xFF`.

use alloc::vec;
use alloc::vec::Vec;

pub trait Flash {
    type Error: core::fmt::Debug;

    /// Bytes one erase sets back to `0xFF`. Erases start and end on multiples of it.
    const ERASE_SIZE: u32;

    /// Programs start on a multiple of it and are a multiple of it long. At most 4.
    const WRITE_SIZE: u32;

    /// Bytes of flash.
    fn capacity(&self) -> u32;

    /// Reads from `&self`: the chain store answers queries from `&self`, and a memory-mapped
    /// flash reads like memory.
    fn read(&self, offset: u32, buf: &mut [u8]) -> Result<(), Self::Error>;

    /// Programs `data` at `offset`: the bits at 0 in `data` are cleared, the others are left as
    /// they are.
    fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), Self::Error>;

    /// Erases `from..to`.
    fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RamFlashError {
    /// Outside the flash, or not aligned to the write or erase size.
    Bounds { offset: u32, len: usize },
    /// A program that would set a bit back to 1, at this offset.
    NotErased(u32),
    /// The simulated power cut happened.
    PowerCut,
}

/// A NOR flash in RAM, with the board's flash geometry (8 KB erases, 2-byte programs). It refuses
/// programs that need an erase first, and can simulate a power cut in the middle of a program or
/// an erase.
#[derive(Clone)]
pub struct RamFlash {
    bytes: Vec<u8>,
    /// Bytes programmed so far.
    programmed: usize,
    /// Bytes left to program before the power cut.
    cut_after: Option<usize>,
    /// The power is cut: programs and erases fail and change nothing.
    off: bool,
}

impl RamFlash {
    /// A new, erased flash.
    pub fn new(capacity: u32) -> Self {
        assert_eq!(capacity % Self::ERASE_SIZE, 0);
        Self {
            bytes: vec![0xFF; capacity as usize],
            programmed: 0,
            cut_after: None,
            off: false,
        }
    }

    /// A flash holding `bytes`, as after a reset.
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        assert_eq!(bytes.len() as u32 % Self::ERASE_SIZE, 0);
        Self {
            bytes,
            programmed: 0,
            cut_after: None,
            off: false,
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Bytes programmed since the flash was made.
    pub fn programmed(&self) -> usize {
        self.programmed
    }

    /// Cuts the power once `bytes` more bytes are programmed: the byte after them is left half
    /// programmed, or the erase that comes then only erases the second half of its range, and
    /// every later program or erase fails.
    pub fn cut_power_after(&mut self, bytes: usize) {
        self.cut_after = Some(bytes);
    }

    fn check(&self, offset: u32, len: usize, align: u32) -> Result<(), RamFlashError> {
        let end = offset as usize + len;
        if end > self.bytes.len() || offset % align != 0 || len as u32 % align != 0 {
            return Err(RamFlashError::Bounds { offset, len });
        }
        Ok(())
    }
}

impl Flash for RamFlash {
    type Error = RamFlashError;
    const ERASE_SIZE: u32 = 8192;
    const WRITE_SIZE: u32 = 2;

    fn capacity(&self) -> u32 {
        self.bytes.len() as u32
    }

    fn read(&self, offset: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
        self.check(offset, buf.len(), 1)?;
        buf.copy_from_slice(&self.bytes[offset as usize..offset as usize + buf.len()]);
        Ok(())
    }

    fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), Self::Error> {
        self.check(offset, data.len(), Self::WRITE_SIZE)?;
        if self.off {
            return Err(RamFlashError::PowerCut);
        }
        for (i, &new) in data.iter().enumerate() {
            let at = offset as usize + i;
            if let Some(left) = &mut self.cut_after {
                if *left == 0 {
                    self.bytes[at] &= new | 0xF0;
                    self.off = true;
                    return Err(RamFlashError::PowerCut);
                }
                *left -= 1;
            }
            self.programmed += 1;
            if self.bytes[at] & new != new {
                return Err(RamFlashError::NotErased(at as u32));
            }
            self.bytes[at] &= new;
        }
        Ok(())
    }

    fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        self.check(from, (to - from) as usize, Self::ERASE_SIZE)?;
        if self.off {
            return Err(RamFlashError::PowerCut);
        }
        if self.cut_after == Some(0) {
            let half = (from as usize + to as usize) / 2;
            self.bytes[half..to as usize].fill(0xFF);
            self.off = true;
            return Err(RamFlashError::PowerCut);
        }
        self.bytes[from as usize..to as usize].fill(0xFF);
        Ok(())
    }
}
