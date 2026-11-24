//! The board's 32 MB SDRAM (IS42S32800J, 32-bit bus) on FMC SDRAM bank 2, at `0xD000_0000`.

pub const BASE: usize = 0xD000_0000;
pub const SIZE: usize = 32 << 20;

/// Checks the data and address lines: walking ones on the data bus, then a distinct word at
/// every power-of-two word offset. Returns the first bad offset in bytes and the word read
/// there.
pub fn check(base: *mut u32) -> Result<(), (usize, u32)> {
    let words = SIZE / 4;
    unsafe {
        for bit in 0..32 {
            let pattern = 1u32 << bit;
            base.write_volatile(pattern);
            let read = base.read_volatile();
            if read != pattern {
                return Err((0, read));
            }
        }

        let marker = |k: usize| 0xA5A5_0000 | k as u32;
        let mut offset = 1;
        let mut k = 0;
        while offset < words {
            base.add(offset).write_volatile(marker(k));
            offset <<= 1;
            k += 1;
        }
        base.write_volatile(!0);
        let mut offset = 1;
        let mut k = 0;
        while offset < words {
            let read = base.add(offset).read_volatile();
            if read != marker(k) {
                return Err((offset * 4, read));
            }
            offset <<= 1;
            k += 1;
        }
    }
    Ok(())
}
