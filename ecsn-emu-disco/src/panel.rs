//! The display's panel on the DSI link: an 800 × 480 MIPI DSI panel with an OTM8009A controller,
//! or an NT35510 as on later boards. It answers the ID reads of its controller and keeps the
//! state of the standard DCS commands (sleep, display on, pixel format, address mode, column and
//! page addresses, brightness); the vendor commands of the init sequences are taken and ignored.
//! The board shows the LTDC's frames while the panel is awake and on, and the DSI host streams
//! them.

use std::sync::{Arc, Mutex};

use stm32h7_rax::devices::dsi::DsiDevice;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Controller {
    Otm8009a,
    Nt35510,
    /// No panel on the link: nothing answers.
    None,
}

impl Controller {
    /// What RDID1, RDID2 and RDID3 answer.
    fn ids(self) -> Option<[u8; 3]> {
        match self {
            Controller::Otm8009a => Some([0x40, 0x00, 0x00]),
            Controller::Nt35510 => Some([0x00, 0x80, 0x00]),
            Controller::None => None,
        }
    }
}

/// The panel's state, which the board reads.
#[derive(Clone, Debug)]
pub struct PanelState {
    pub controller: Controller,
    pub awake: bool,
    pub on: bool,
    /// MADCTL: bit 5 (MV) exchanges rows and columns: landscape.
    pub address_mode: u8,
    /// COLMOD: 0x55 RGB565, 0x77 RGB888.
    pub pixel_format: u8,
    pub columns: (u16, u16),
    pub pages: (u16, u16),
    pub brightness: u8,
    /// DCS commands taken since the reset.
    pub commands: u32,
}

impl PanelState {
    fn reset(controller: Controller) -> Self {
        Self {
            controller,
            awake: false,
            on: false,
            address_mode: 0,
            pixel_format: 0x77,
            columns: (0, 479),
            pages: (0, 799),
            brightness: 0,
            commands: 0,
        }
    }

    /// Whether the panel shows what it is sent.
    pub fn showing(&self) -> bool {
        self.awake && self.on
    }
}

/// DSI data types: DCS short writes, DCS long write, DCS read.
const DCS_SHORT_WRITE_0: u8 = 0x05;
const DCS_SHORT_WRITE_1: u8 = 0x15;
const DCS_LONG_WRITE: u8 = 0x39;
const DCS_READ: u8 = 0x06;

pub struct Panel {
    pub state: Arc<Mutex<PanelState>>,
}

impl Panel {
    pub fn new(controller: Controller) -> Self {
        Self {
            state: Arc::new(Mutex::new(PanelState::reset(controller))),
        }
    }

    /// The panel held in reset (its XRES low).
    pub fn reset(state: &Mutex<PanelState>) {
        let mut s = state.lock().unwrap();
        *s = PanelState::reset(s.controller);
    }
}

fn pair(bytes: &[u8]) -> Option<(u16, u16)> {
    match bytes {
        [a, b, c, d, ..] => Some((u16::from_be_bytes([*a, *b]), u16::from_be_bytes([*c, *d]))),
        _ => None,
    }
}

impl DsiDevice for Panel {
    fn packet(&mut self, data_type: u8, _channel: u8, payload: &[u8]) -> Option<Vec<u8>> {
        let mut s = self.state.lock().unwrap();
        let (command, parameters) = match data_type {
            DCS_SHORT_WRITE_0 => (payload[0], &[][..]),
            DCS_SHORT_WRITE_1 => (payload[0], &payload[1..2]),
            DCS_LONG_WRITE => match payload.split_first() {
                Some((c, p)) => (*c, p),
                None => return None,
            },
            DCS_READ => {
                let ids = s.controller.ids()?;
                return match payload[0] {
                    0xDA => Some(vec![ids[0]]),
                    0xDB => Some(vec![ids[1]]),
                    0xDC => Some(vec![ids[2]]),
                    // RDDPM: booster on, sleep out, display on
                    0x0A => Some(vec![
                        0x80 | (u8::from(s.awake) << 4) | (u8::from(s.on) << 2),
                    ]),
                    _ => None,
                };
            }
            _ => return None,
        };
        s.commands += 1;
        let first = parameters.first().copied().unwrap_or(0);
        match command {
            0x10 => s.awake = false,
            0x11 => s.awake = true,
            0x28 => s.on = false,
            0x29 => s.on = true,
            0x2A => s.columns = pair(parameters).unwrap_or(s.columns),
            0x2B => s.pages = pair(parameters).unwrap_or(s.pages),
            0x36 => s.address_mode = first,
            0x3A => s.pixel_format = first,
            0x51 => s.brightness = first,
            _ => {}
        }
        None
    }
}
