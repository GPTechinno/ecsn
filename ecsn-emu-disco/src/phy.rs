//! The LAN8742A Ethernet PHY at MDIO address 0: the IEEE 802.3 clause 22
//! registers, its identifier, a link always up at 100 Mbit/s full duplex
//! once autonegotiation is done.

use stm32h7_rax::devices::eth::Mdio;

const ADDRESS: u8 = 0;
const BMCR: u8 = 0;
const BMSR: u8 = 1;
const PHYID1: u8 = 2;
const PHYID2: u8 = 3;
const ANAR: u8 = 4;
const ANLPAR: u8 = 5;
/// PHY special control/status: speed indication 100BASE-TX full duplex,
/// autodone.
const PSCSR: u8 = 31;

const BMCR_RESET: u16 = 1 << 15;
/// 100BASE-TX full and half, 10BASE-T full and half, autonegotiation
/// ability, link status, autonegotiation complete, extended capabilities.
const BMSR_UP: u16 = 0x7800 | (1 << 5) | (1 << 3) | (1 << 2) | 1;

pub struct Lan8742 {
    regs: [u16; 32],
}

impl Lan8742 {
    pub fn new() -> Self {
        let mut regs = [0; 32];
        regs[BMCR as usize] = 0x3100;
        regs[PHYID1 as usize] = 0x0007;
        regs[PHYID2 as usize] = 0xC131;
        regs[ANAR as usize] = 0x01E1;
        regs[ANLPAR as usize] = 0x45E1;
        regs[PSCSR as usize] = (0b110 << 2) | (1 << 12);
        Self { regs }
    }
}

impl Mdio for Lan8742 {
    fn read(&mut self, phy: u8, reg: u8) -> u16 {
        if phy != ADDRESS {
            return 0xFFFF;
        }
        match reg {
            BMSR => BMSR_UP,
            r => self.regs[r as usize & 31],
        }
    }

    fn write(&mut self, phy: u8, reg: u8, value: u16) {
        if phy != ADDRESS {
            return;
        }
        match reg {
            // the reset completes at once; restarting autonegotiation too
            BMCR if value & BMCR_RESET != 0 => *self = Self::new(),
            // restarting autonegotiation (bit 9) completes at once
            BMCR => self.regs[BMCR as usize] = value & !(1 << 9),
            BMSR | PHYID1 | PHYID2 => {}
            r => self.regs[r as usize & 31] = value,
        }
    }
}
