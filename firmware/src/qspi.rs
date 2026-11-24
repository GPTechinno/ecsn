//! The board's QSPI NOR flash: two MT25QL512ABB (64 MB each) on the QUADSPI in dual-flash mode,
//! seen as one 128 MB flash whose even bytes are in the first chip and odd bytes in the second.
//! The QUADSPI halves the address for each chip, so pages (256 bytes), subsectors (4 KB) and
//! sectors (64 KB) of a chip are twice as large here.
//!
//! Reads go through the memory map at `0x9000_0000`. Programs and erases leave it for indirect
//! mode, wait for both chips (busy-waiting: a subsector erase takes up to 400 ms), then go back.
//!
//! The commands are those with 4-byte addresses, which work in either address mode, with the
//! instruction on one line: after their reset, the chips are in that mode.

use ecsn::flash::Flash;
use embassy_stm32::pac;
use embassy_stm32::pac::gpio::vals::{Moder, Ospeedr, Ot, Pupdr};
use embassy_stm32::peripherals::{PB2, PD11, PF6, PF7, PF9, PG14, PG6, PG9, PH2, PH3, QUADSPI};
use embassy_stm32::Peri;
use embassy_time::{block_for, Duration, Instant};

/// The QUADSPI's memory-mapped window.
pub const WINDOW: usize = 0x9000_0000;
pub const WINDOW_SIZE: usize = 256 << 20;

const CAPACITY: u32 = 128 << 20;
/// A page of each chip.
const PAGE: u32 = 512;
/// A subsector of each chip, the smallest erase.
const SUBSECTOR: u32 = 8 << 10;
/// A sector of each chip.
const SECTOR: u32 = 128 << 10;

const RESET_ENABLE: u8 = 0x66;
const RESET_MEMORY: u8 = 0x99;
const WRITE_ENABLE: u8 = 0x06;
const READ_STATUS: u8 = 0x05;
const READ_FLAG_STATUS: u8 = 0x70;
const CLEAR_FLAG_STATUS: u8 = 0x50;
/// 4-byte quad output fast read: instruction and address on one line, data on four.
const QUAD_OUTPUT_FAST_READ: u8 = 0x6C;
/// The chips' default for the fast reads.
const READ_DUMMY_CYCLES: u8 = 8;
/// 4-byte quad input fast program: instruction and address on one line, data on four.
const QUAD_INPUT_FAST_PROGRAM: u8 = 0x34;
const SUBSECTOR_ERASE: u8 = 0x21;
const SECTOR_ERASE: u8 = 0xDC;
const BULK_ERASE: u8 = 0xC7;

const STATUS_BUSY: u8 = 1 << 0;
const STATUS_WRITE_ENABLED: u8 = 1 << 1;
/// Flag status: erase, program and protection errors.
const FLAG_ERRORS: u8 = 1 << 5 | 1 << 4 | 1 << 1;

// the longest times of the datasheet, and some margin
const PROGRAM_TIME: Duration = Duration::from_millis(10);
const SUBSECTOR_ERASE_TIME: Duration = Duration::from_millis(1000);
const SECTOR_ERASE_TIME: Duration = Duration::from_millis(3000);
const BULK_ERASE_TIME: Duration = Duration::from_secs(600);

// QUADSPI_CCR fields
const LINES_NONE: u8 = 0;
const LINES_ONE: u8 = 1;
const LINES_FOUR: u8 = 3;
const ADDRESS_32_BITS: u8 = 3;
const INDIRECT_WRITE: u8 = 0;
const INDIRECT_READ: u8 = 1;
const MEMORY_MAPPED: u8 = 3;
const FIFO_BYTES: u8 = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, defmt::Format)]
pub enum Error {
    /// Outside the flash, or not aligned to the write or erase size.
    Bounds,
    /// The chips stayed busy past the operation's longest time.
    Timeout,
    /// The flag status register of each chip after a failed program or erase.
    Failed(u8, u8),
}

pub struct QspiFlash {
    _quadspi: Peri<'static, QUADSPI>,
}

/// One QUADSPI command in indirect mode.
#[derive(Clone, Copy)]
struct Command {
    instruction: u8,
    instruction_lines: u8,
    address: Option<u32>,
    data_lines: u8,
}

impl Command {
    const fn new(instruction: u8) -> Self {
        Self {
            instruction,
            instruction_lines: LINES_ONE,
            address: None,
            data_lines: LINES_NONE,
        }
    }
}

fn regs() -> pac::quadspi::Quadspi {
    pac::QUADSPI
}

/// The data register, for one-byte accesses to the FIFO.
fn data_register() -> *mut u8 {
    regs().dr().as_ptr() as *mut u8
}

impl QspiFlash {
    /// Sets up the QUADSPI at 50 MHz, resets the chips (from either single or quad instruction
    /// mode), and maps the flash.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        quadspi: Peri<'static, QUADSPI>,
        _clk: Peri<'static, PB2>,
        _ncs: Peri<'static, PG6>,
        _bk1_io0: Peri<'static, PD11>,
        _bk1_io1: Peri<'static, PF9>,
        _bk1_io2: Peri<'static, PF7>,
        _bk1_io3: Peri<'static, PF6>,
        _bk2_io0: Peri<'static, PH2>,
        _bk2_io1: Peri<'static, PH3>,
        _bk2_io2: Peri<'static, PG9>,
        _bk2_io3: Peri<'static, PG14>,
    ) -> Result<Self, Error> {
        pac::RCC.ahb4enr().modify(|w| {
            w.set_gpioben(true);
            w.set_gpioden(true);
            w.set_gpiofen(true);
            w.set_gpiogen(true);
            w.set_gpiohen(true);
        });
        pac::RCC.ahb3enr().modify(|w| w.set_quadspien(true));
        pac::RCC.ahb3rstr().modify(|w| w.set_quadspirst(true));
        pac::RCC.ahb3rstr().modify(|w| w.set_quadspirst(false));

        alternate(pac::GPIOB, 2, 9, Pupdr::FLOATING); // CLK
        alternate(pac::GPIOG, 6, 10, Pupdr::PULL_UP); // BK1_NCS, both chips
        alternate(pac::GPIOD, 11, 9, Pupdr::FLOATING); // BK1_IO0
        alternate(pac::GPIOF, 9, 10, Pupdr::FLOATING); // BK1_IO1
        alternate(pac::GPIOF, 7, 9, Pupdr::FLOATING); // BK1_IO2
        alternate(pac::GPIOF, 6, 9, Pupdr::FLOATING); // BK1_IO3
        alternate(pac::GPIOH, 2, 9, Pupdr::FLOATING); // BK2_IO0
        alternate(pac::GPIOH, 3, 9, Pupdr::FLOATING); // BK2_IO1
        alternate(pac::GPIOG, 9, 9, Pupdr::FLOATING); // BK2_IO2
        alternate(pac::GPIOG, 14, 9, Pupdr::FLOATING); // BK2_IO3

        let r = regs();
        // the start-up sequence of the QUADSPI errata
        r.cr().write_value(pac::quadspi::regs::Cr(0));
        while r.sr().read().busy() {}
        r.cr().write_value(pac::quadspi::regs::Cr(0xFF00_0001));
        r.ccr().write(|w| w.set_frcm(true));
        r.ccr().write(|w| w.set_frcm(true));
        r.cr().write_value(pac::quadspi::regs::Cr(0));
        while r.sr().read().busy() {}

        r.dcr().write(|w| {
            w.set_fsize(CAPACITY.trailing_zeros() as u8 - 1);
            w.set_csht(3); // 4 cycles, 80 ns
            w.set_ckmode(false);
        });
        r.cr().write(|w| {
            w.set_prescaler(3); // 200 MHz / 4
            w.set_sshift(true);
            w.set_dfm(true);
            w.set_fsel(false);
            w.set_fthres(0);
            w.set_en(true);
        });

        let mut flash = Self { _quadspi: quadspi };
        for lines in [LINES_FOUR, LINES_ONE] {
            for instruction in [RESET_ENABLE, RESET_MEMORY] {
                flash.command(Command {
                    instruction_lines: lines,
                    ..Command::new(instruction)
                });
            }
        }
        block_for(Duration::from_millis(1));
        flash.wait_ready(PROGRAM_TIME)?;
        flash.map();
        Ok(flash)
    }

    fn map(&mut self) {
        let r = regs();
        while r.sr().read().busy() {}
        clear_flags();
        r.ccr().write(|w| {
            w.set_fmode(MEMORY_MAPPED);
            w.set_imode(LINES_ONE);
            w.set_instruction(QUAD_OUTPUT_FAST_READ);
            w.set_admode(LINES_ONE);
            w.set_adsize(ADDRESS_32_BITS);
            w.set_dmode(LINES_FOUR);
            w.set_dcyc(READ_DUMMY_CYCLES);
        });
    }

    fn unmap(&mut self) {
        let r = regs();
        r.cr().modify(|w| w.set_abort(true));
        while r.cr().read().abort() {}
        while r.sr().read().busy() {}
    }

    /// Starts `command` in indirect mode, with `len` bytes of data if any.
    fn start(&mut self, command: Command, fmode: u8, len: usize) {
        let r = regs();
        while r.sr().read().busy() {}
        clear_flags();
        if len > 0 {
            r.dlr().write(|w| w.set_dl(len as u32 - 1));
        }
        r.ccr().write(|w| {
            w.set_fmode(fmode);
            w.set_imode(command.instruction_lines);
            w.set_instruction(command.instruction);
            w.set_admode(if command.address.is_some() {
                LINES_ONE
            } else {
                LINES_NONE
            });
            w.set_adsize(ADDRESS_32_BITS);
            w.set_dmode(command.data_lines);
        });
        if let Some(address) = command.address {
            r.ar().write(|w| w.set_address(address));
        }
    }

    fn wait_transfer(&mut self) {
        let r = regs();
        while !r.sr().read().tcf() {}
        r.fcr().write(|w| w.set_ctcf(true));
    }

    /// A command without data.
    fn command(&mut self, command: Command) {
        self.start(command, INDIRECT_WRITE, 0);
        self.wait_transfer();
    }

    /// A one-byte register of each chip.
    fn read_register(&mut self, instruction: u8) -> [u8; 2] {
        let command = Command {
            data_lines: LINES_ONE,
            ..Command::new(instruction)
        };
        self.start(command, INDIRECT_READ, 2);
        self.wait_transfer();
        unsafe {
            [
                data_register().read_volatile(),
                data_register().read_volatile(),
            ]
        }
    }

    fn wait_ready(&mut self, longest: Duration) -> Result<(), Error> {
        let deadline = Instant::now() + longest;
        loop {
            let [first, second] = self.read_register(READ_STATUS);
            if (first | second) & STATUS_BUSY == 0 {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(Error::Timeout);
            }
        }
    }

    fn write_enable(&mut self) -> Result<(), Error> {
        self.command(Command::new(WRITE_ENABLE));
        let deadline = Instant::now() + PROGRAM_TIME;
        loop {
            let [first, second] = self.read_register(READ_STATUS);
            if first & second & STATUS_WRITE_ENABLED != 0 {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(Error::Timeout);
            }
        }
    }

    /// Checks that the last program or erase succeeded on both chips.
    fn check(&mut self) -> Result<(), Error> {
        let [first, second] = self.read_register(READ_FLAG_STATUS);
        if (first | second) & FLAG_ERRORS != 0 {
            self.command(Command::new(CLEAR_FLAG_STATUS));
            return Err(Error::Failed(first, second));
        }
        Ok(())
    }

    /// Programs `data`, within one page.
    fn program(&mut self, address: u32, data: &[u8]) -> Result<(), Error> {
        self.write_enable()?;
        let command = Command {
            address: Some(address),
            data_lines: LINES_FOUR,
            ..Command::new(QUAD_INPUT_FAST_PROGRAM)
        };
        self.start(command, INDIRECT_WRITE, data.len());
        for &byte in data {
            while regs().sr().read().flevel() >= FIFO_BYTES {}
            unsafe { data_register().write_volatile(byte) };
        }
        self.wait_transfer();
        self.wait_ready(PROGRAM_TIME)?;
        self.check()
    }

    fn erase_at(
        &mut self,
        instruction: u8,
        address: Option<u32>,
        longest: Duration,
    ) -> Result<(), Error> {
        self.write_enable()?;
        self.command(Command {
            address,
            ..Command::new(instruction)
        });
        self.wait_ready(longest)?;
        self.check()
    }

    fn write_unmapped(&mut self, mut offset: u32, mut data: &[u8]) -> Result<(), Error> {
        while !data.is_empty() {
            let len = data.len().min((PAGE - offset % PAGE) as usize);
            self.program(offset, &data[..len])?;
            offset += len as u32;
            data = &data[len..];
        }
        Ok(())
    }

    fn erase_unmapped(&mut self, from: u32, to: u32) -> Result<(), Error> {
        if from == 0 && to == CAPACITY {
            return self.erase_at(BULK_ERASE, None, BULK_ERASE_TIME);
        }
        let mut at = from;
        while at < to {
            if at % SECTOR == 0 && to - at >= SECTOR {
                self.erase_at(SECTOR_ERASE, Some(at), SECTOR_ERASE_TIME)?;
                at += SECTOR;
            } else {
                self.erase_at(SUBSECTOR_ERASE, Some(at), SUBSECTOR_ERASE_TIME)?;
                at += SUBSECTOR;
            }
        }
        Ok(())
    }
}

fn clear_flags() {
    regs().fcr().write(|w| {
        w.set_ctef(true);
        w.set_ctcf(true);
        w.set_csmf(true);
        w.set_ctof(true);
    });
}

/// Gives pin `n` of `port` to alternate function `af`, at the highest speed.
fn alternate(port: pac::gpio::Gpio, n: usize, af: u8, pull: Pupdr) {
    port.afr(n / 8).modify(|w| w.set_afr(n % 8, af));
    port.ospeedr()
        .modify(|w| w.set_ospeedr(n, Ospeedr::VERY_HIGH_SPEED));
    port.otyper().modify(|w| w.set_ot(n, Ot::PUSH_PULL));
    port.pupdr().modify(|w| w.set_pupdr(n, pull));
    port.moder().modify(|w| w.set_moder(n, Moder::ALTERNATE));
}

impl Flash for QspiFlash {
    type Error = Error;
    const ERASE_SIZE: u32 = SUBSECTOR;
    const WRITE_SIZE: u32 = 2;

    fn capacity(&self) -> u32 {
        CAPACITY
    }

    fn read(&self, offset: u32, buf: &mut [u8]) -> Result<(), Error> {
        if offset as u64 + buf.len() as u64 > CAPACITY as u64 {
            return Err(Error::Bounds);
        }
        // device memory: aligned accesses only
        let mut at = WINDOW + offset as usize;
        let mut i = 0;
        let read_byte = |at: usize| unsafe { (at as *const u8).read_volatile() };
        while i < buf.len() && at % 4 != 0 {
            buf[i] = read_byte(at);
            (at, i) = (at + 1, i + 1);
        }
        while buf.len() - i >= 4 {
            let word = unsafe { (at as *const u32).read_volatile() };
            buf[i..i + 4].copy_from_slice(&word.to_le_bytes());
            (at, i) = (at + 4, i + 4);
        }
        while i < buf.len() {
            buf[i] = read_byte(at);
            (at, i) = (at + 1, i + 1);
        }
        Ok(())
    }

    fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), Error> {
        if offset as u64 + data.len() as u64 > CAPACITY as u64
            || offset % Self::WRITE_SIZE != 0
            || data.len() as u32 % Self::WRITE_SIZE != 0
        {
            return Err(Error::Bounds);
        }
        self.unmap();
        let result = self.write_unmapped(offset, data);
        self.map();
        result
    }

    fn erase(&mut self, from: u32, to: u32) -> Result<(), Error> {
        if from > to || to > CAPACITY || from % SUBSECTOR != 0 || to % SUBSECTOR != 0 {
            return Err(Error::Bounds);
        }
        self.unmap();
        let result = self.erase_unmapped(from, to);
        self.map();
        result
    }
}
