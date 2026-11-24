//! Memory attributes of the external memories.

use cortex_m::peripheral::{MPU, SCB};

use crate::{qspi, sdram};

// RASR
const XN: u32 = 1 << 28;
const AP_FULL_ACCESS: u32 = 0b011 << 24;
const TEX_WRITE_ALLOCATE: u32 = 0b001 << 19;
const S: u32 = 1 << 18;
const C: u32 = 1 << 17;
const B: u32 = 1 << 16;
const ENABLE: u32 = 1;

/// The RASR size field of a region of `bytes`, a power of two: log2(bytes) - 1.
fn size(bytes: usize) -> u32 {
    assert!(bytes.is_power_of_two());
    (bytes.trailing_zeros() - 1) << 1
}

/// - The SDRAM as normal memory, write-back cacheable: the default memory map makes
///   `0xC000_0000..0xE000_0000` device memory, where unaligned accesses fault.
/// - The QSPI flash window as device memory: the default normal memory there lets the core read
///   it speculatively, which stalls the QUADSPI when it is not in memory-mapped mode.
pub fn configure(mpu: &mut MPU, scb: &mut SCB) {
    const MEMFAULTENA: u32 = 1 << 16;
    const PRIVDEFENA: u32 = 1 << 2;
    const MPU_ENABLE: u32 = 1;

    let regions = [
        (sdram::BASE, TEX_WRITE_ALLOCATE | C | B | size(sdram::SIZE)),
        (qspi::WINDOW, S | B | size(qspi::WINDOW_SIZE)),
    ];
    unsafe {
        cortex_m::asm::dmb();
        scb.shcsr.modify(|r| r & !MEMFAULTENA);
        mpu.ctrl.write(0);
        for (n, (base, attributes)) in regions.into_iter().enumerate() {
            mpu.rnr.write(n as u32);
            mpu.rbar.write(base as u32);
            mpu.rasr.write(XN | AP_FULL_ACCESS | attributes | ENABLE);
        }
        mpu.ctrl.write(PRIVDEFENA | MPU_ENABLE);
        scb.shcsr.modify(|r| r | MEMFAULTENA);
        cortex_m::asm::dsb();
        cortex_m::asm::isb();
    }
}
