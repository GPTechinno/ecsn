//! The board's 32 MB SDRAM (IS42S32800J, 32-bit bus) on FMC SDRAM bank 2, at `0xD000_0000`.

use cortex_m::peripheral::{MPU, SCB};

pub const BASE: usize = 0xD000_0000;
pub const SIZE: usize = 32 << 20;

/// Maps the SDRAM as normal memory, write-back cacheable. The default memory map makes
/// `0xC000_0000..0xE000_0000` device memory, where unaligned accesses fault.
pub fn map(mpu: &mut MPU, scb: &mut SCB) {
    const MEMFAULTENA: u32 = 1 << 16;
    // RASR
    const XN: u32 = 1 << 28;
    const AP_FULL_ACCESS: u32 = 0b011 << 24;
    const TEX_WRITE_ALLOCATE: u32 = 0b001 << 19;
    const C: u32 = 1 << 17;
    const B: u32 = 1 << 16;
    const ENABLE: u32 = 1;
    // CTRL
    const PRIVDEFENA: u32 = 1 << 2;
    const MPU_ENABLE: u32 = 1;

    // SIZE is log2(bytes) - 1
    let size = SIZE.trailing_zeros() - 1;
    unsafe {
        cortex_m::asm::dmb();
        scb.shcsr.modify(|r| r & !MEMFAULTENA);
        mpu.ctrl.write(0);
        mpu.rnr.write(0);
        mpu.rbar.write(BASE as u32);
        mpu.rasr
            .write(XN | AP_FULL_ACCESS | TEX_WRITE_ALLOCATE | C | B | (size << 1) | ENABLE);
        mpu.ctrl.write(PRIVDEFENA | MPU_ENABLE);
        scb.shcsr.modify(|r| r | MEMFAULTENA);
        cortex_m::asm::dsb();
        cortex_m::asm::isb();
    }
}

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
