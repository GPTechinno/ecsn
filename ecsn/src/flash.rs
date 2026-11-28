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

/// The magic of a saved record ([`save_record`]).
const RECORD_MAGIC: [u8; 8] = *b"ECSNSAVE";
const RECORD_HEADER: usize = 16;

/// Saves `data` in the erase blocks `at..at + len` of `flash`, erased first: a header (magic,
/// length, CRC-32) then the bytes. A power cut while it saves leaves no record, not a wrong one.
/// Returns `false`, saving nothing, if it does not fit.
pub fn save_record<F: Flash>(
    flash: &mut F,
    at: u32,
    len: u32,
    data: &[u8],
) -> Result<bool, F::Error> {
    let w = F::WRITE_SIZE as usize;
    let total = (RECORD_HEADER + data.len()).div_ceil(w) * w;
    if total > len as usize {
        return Ok(false);
    }
    let mut bytes = vec![0xFF; total];
    bytes[..8].copy_from_slice(&RECORD_MAGIC);
    bytes[8..12].copy_from_slice(&(data.len() as u32).to_le_bytes());
    bytes[12..16].copy_from_slice(&crate::store::crc32(&[data]).to_le_bytes());
    bytes[RECORD_HEADER..RECORD_HEADER + data.len()].copy_from_slice(data);
    flash.erase(at, at + len)?;
    // the bytes, then the header: a record is whole once its magic is written
    if total > RECORD_HEADER {
        flash.write(at + RECORD_HEADER as u32, &bytes[RECORD_HEADER..])?;
    }
    flash.write(at, &bytes[..RECORD_HEADER])?;
    Ok(true)
}

/// The record [`save_record`] left in `at..at + len`, if one is there whole.
pub fn load_record<F: Flash>(flash: &F, at: u32, len: u32) -> Option<Vec<u8>> {
    let mut header = [0; RECORD_HEADER];
    flash.read(at, &mut header).ok()?;
    if header[..8] != RECORD_MAGIC {
        return None;
    }
    let size = u32::from_le_bytes(header[8..12].try_into().ok()?) as usize;
    let crc = u32::from_le_bytes(header[12..16].try_into().ok()?);
    if RECORD_HEADER + size > len as usize {
        return None;
    }
    let mut data = vec![0; size];
    flash.read(at + RECORD_HEADER as u32, &mut data).ok()?;
    (crate::store::crc32(&[&data]) == crc).then_some(data)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_record_comes_back_whole_or_not_at_all() {
        let mut flash = RamFlash::new(64 * 1024);
        let (at, len) = (48 * 1024, 16 * 1024);
        assert_eq!(load_record(&flash, at, len), None);
        assert!(save_record(&mut flash, at, len, b"the book").unwrap());
        assert_eq!(
            load_record(&flash, at, len).as_deref(),
            Some(&b"the book"[..])
        );
        // too large: nothing saved, the record before kept
        assert!(!save_record(&mut flash, at, len, &[0; 17 * 1024]).unwrap());
        assert_eq!(
            load_record(&flash, at, len).as_deref(),
            Some(&b"the book"[..])
        );
        // a power cut while it saves: no record after the reset, not a wrong one
        let mut cut = flash.clone();
        cut.cut_power_after(4);
        assert!(save_record(&mut cut, at, len, b"another book").is_err());
        let after = RamFlash::from_bytes(cut.bytes().to_vec());
        assert_eq!(load_record(&after, at, len), None);
    }
}
