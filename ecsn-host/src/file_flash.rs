//! A NOR flash image in a file, with the board's flash geometry. Like the flash, it refuses to
//! program a bit back to 1.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

use ecsn::flash::Flash;

pub struct FileFlash {
    file: File,
    capacity: u32,
}

impl FileFlash {
    /// Opens the image at `path`, or creates an erased one of `capacity` bytes.
    pub fn open(path: &Path, capacity: u32) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let len = file.metadata()?.len();
        let mut flash = Self { file, capacity };
        if len == 0 {
            flash.erase(0, capacity)?;
        } else {
            flash.capacity = u32::try_from(len)
                .ok()
                .filter(|len| len % Self::ERASE_SIZE == 0)
                .ok_or_else(|| io::Error::other("not a flash image"))?;
        }
        Ok(flash)
    }
}

impl Flash for FileFlash {
    type Error = io::Error;
    const ERASE_SIZE: u32 = 8192;
    const WRITE_SIZE: u32 = 2;

    fn capacity(&self) -> u32 {
        self.capacity
    }

    fn read(&self, offset: u32, buf: &mut [u8]) -> io::Result<()> {
        self.file.read_exact_at(buf, offset as u64)
    }

    fn write(&mut self, offset: u32, data: &[u8]) -> io::Result<()> {
        let mut old = vec![0; data.len()];
        self.read(offset, &mut old)?;
        if let Some(i) = (0..data.len()).find(|&i| old[i] & data[i] != data[i]) {
            return Err(io::Error::other(format!(
                "programming bits back to 1 at {}",
                offset as usize + i
            )));
        }
        self.file.write_all_at(data, offset as u64)
    }

    fn erase(&mut self, from: u32, to: u32) -> io::Result<()> {
        let blank = vec![0xFF; 1 << 20];
        let mut at = from;
        while at < to {
            let len = (to - at).min(blank.len() as u32);
            self.file.write_all_at(&blank[..len as usize], at as u64)?;
            at += len;
        }
        Ok(())
    }
}
