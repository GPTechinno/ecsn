//! The board's 4" display: an 800 × 480 MIPI DSI panel whose controller is an OTM8009A or, on
//! later boards, an NT35510, told apart by their IDs. The LTDC reads a frame buffer in SDRAM
//! (RGB565) and the DSI host streams it to the panel in video burst mode on two data lanes. Two
//! frame buffers: one is drawn while the LTDC shows the other, swapped at a vertical blanking.
//!
//! The clocks, timings and the controllers' init sequences are those of ST's board support
//! package, which uses the NT35510's timings with either controller.
//!
//! The D-cache is off: the LTDC sees what the core writes at once. With the D-cache on, a frame
//! buffer would have to be cleaned from the cache before it is shown.

use alloc::alloc::{alloc_zeroed, dealloc, Layout};

use embassy_stm32::gpio::Output;
use embassy_stm32::pac::ltdc::vals::{Bf1, Bf2, Depol, Hspol, Imr, Pcpol, Pf, Vbr, Vspol};
use embassy_stm32::pac::{DSIHOST, LTDC};
use embassy_stm32::{peripherals, rcc, Peri};
use embassy_time::{Duration, Instant, Timer};

pub const WIDTH: usize = 800;
pub const HEIGHT: usize = 480;
const PIXELS: usize = WIDTH * HEIGHT;

/// Video timings, in pixels and lines.
const HSYNC: u16 = 2;
const HBP: u16 = 34;
const HFP: u16 = 34;
const VSYNC: u16 = 120;
const VBP: u16 = 150;
const VFP: u16 = 150;
/// The DSI lane byte clock (the DSI PLL's 500 MHz over 8) and the pixel clock, kHz, which turn
/// horizontal timings into lane byte clock cycles.
const LANE_BYTE_KHZ: u32 = 62_500;
const PIXEL_KHZ: u32 = 27_429;

/// DSI data types: DCS short writes without and with a parameter, DCS long write, DCS read.
const DCS_SHORT_WRITE_0: u8 = 0x05;
const DCS_SHORT_WRITE_1: u8 = 0x15;
const DCS_LONG_WRITE: u8 = 0x39;
const DCS_READ: u8 = 0x06;
/// DCS commands: read ID1, read ID2.
const RDID1: u8 = 0xDA;
const RDID2: u8 = 0xDB;
const OTM8009A_ID1: u8 = 0x40;
const NT35510_ID2: u8 = 0x80;

/// How long the DSI host may take to take a command or to bring an answer.
const DSI_TIMEOUT: Duration = Duration::from_millis(20);

#[derive(Clone, Copy, Debug, PartialEq, Eq, defmt::Format)]
pub enum Controller {
    Otm8009a,
    Nt35510,
}

#[derive(Clone, Copy, Debug, defmt::Format)]
pub enum Error {
    /// The DSI regulator did not become ready.
    Regulator,
    /// The DSI PLL did not lock.
    Pll,
    /// The DSI host kept a command, or the panel did not answer one.
    Timeout,
    /// The panel answered with IDs of neither controller.
    UnknownPanel { id1: u8, id2: u8 },
}

/// A step of an init sequence: a DCS command and its parameters (one at least: a command
/// without parameters is sent with a zero), or a pause in milliseconds.
enum Step {
    Command(u8, &'static [u8]),
    Pause(u64),
}

use Step::{Command as C, Pause};

/// The OTM8009A's init: its vendor registers (`0x00 n` sets the low byte of the next one's
/// address), sleep out, RGB565, landscape, brightness, display on, then memory write, which
/// starts taking the video stream.
const OTM8009A_INIT: &[Step] = &[
    C(0x00, &[0x00]),
    C(0xFF, &[0x80, 0x09, 0x01]),
    C(0x00, &[0x80]),
    C(0xFF, &[0x80, 0x09]),
    C(0x00, &[0x80]),
    C(0xC4, &[0x30]),
    Pause(10),
    C(0x00, &[0x8A]),
    C(0xC4, &[0x40]),
    Pause(10),
    C(0x00, &[0xB1]),
    C(0xC5, &[0xA9]),
    C(0x00, &[0x91]),
    C(0xC5, &[0x34]),
    C(0x00, &[0xB4]),
    C(0xC0, &[0x50]),
    C(0x00, &[0x00]),
    C(0xD9, &[0x4E]),
    C(0x00, &[0x81]),
    C(0xC1, &[0x66]),
    C(0x00, &[0xA1]),
    C(0xC1, &[0x08]),
    C(0x00, &[0x92]),
    C(0xC5, &[0x01]),
    C(0x00, &[0x95]),
    C(0xC5, &[0x34]),
    C(0x00, &[0x00]),
    C(0xD8, &[0x79, 0x79]),
    C(0x00, &[0x94]),
    C(0xC5, &[0x33]),
    C(0x00, &[0xA3]),
    C(0xC0, &[0x1B]),
    C(0x00, &[0x82]),
    C(0xC5, &[0x83]),
    C(0x00, &[0x81]),
    C(0xC4, &[0x83]),
    C(0x00, &[0xA1]),
    C(0xC1, &[0x0E]),
    C(0x00, &[0xA6]),
    C(0xB3, &[0x00, 0x01]),
    C(0x00, &[0x80]),
    C(0xCE, &[0x85, 0x01, 0x00, 0x84, 0x01, 0x00]),
    C(0x00, &[0xA0]),
    C(
        0xCE,
        &[
            0x18, 0x04, 0x03, 0x39, 0x00, 0x00, 0x00, 0x18, 0x03, 0x03, 0x3A, 0x00, 0x00, 0x00,
        ],
    ),
    C(0x00, &[0xB0]),
    C(
        0xCE,
        &[
            0x18, 0x02, 0x03, 0x3B, 0x00, 0x00, 0x00, 0x18, 0x01, 0x03, 0x3C, 0x00, 0x00, 0x00,
        ],
    ),
    C(0x00, &[0xC0]),
    C(
        0xCF,
        &[0x01, 0x01, 0x20, 0x20, 0x00, 0x00, 0x01, 0x02, 0x00, 0x00],
    ),
    C(0x00, &[0xD0]),
    C(0xCF, &[0x00]),
    C(0x00, &[0x80]),
    C(0xCB, &[0x00; 10]),
    C(0x00, &[0x90]),
    C(0xCB, &[0x00; 15]),
    C(0x00, &[0xA0]),
    C(0xCB, &[0x00; 15]),
    C(0x00, &[0xB0]),
    C(0xCB, &[0x00; 10]),
    C(0x00, &[0xC0]),
    C(
        0xCB,
        &[
            0x00, 0x04, 0x04, 0x04, 0x04, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00,
        ],
    ),
    C(0x00, &[0xD0]),
    C(
        0xCB,
        &[
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x04, 0x04, 0x04, 0x04, 0x00, 0x00, 0x00,
            0x00,
        ],
    ),
    C(0x00, &[0xE0]),
    C(0xCB, &[0x00; 10]),
    C(0x00, &[0xF0]),
    C(0xCB, &[0xFF; 10]),
    C(0x00, &[0x80]),
    C(
        0xCC,
        &[0x00, 0x26, 0x09, 0x0B, 0x01, 0x25, 0x00, 0x00, 0x00, 0x00],
    ),
    C(0x00, &[0x90]),
    C(
        0xCC,
        &[
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x26, 0x0A, 0x0C,
            0x02,
        ],
    ),
    C(0x00, &[0xA0]),
    C(
        0xCC,
        &[
            0x25, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00,
        ],
    ),
    C(0x00, &[0xB0]),
    C(
        0xCC,
        &[0x00, 0x25, 0x0C, 0x0A, 0x02, 0x26, 0x00, 0x00, 0x00, 0x00],
    ),
    C(0x00, &[0xC0]),
    C(
        0xCC,
        &[
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x25, 0x0B, 0x09,
            0x01,
        ],
    ),
    C(0x00, &[0xD0]),
    C(
        0xCC,
        &[
            0x26, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00,
        ],
    ),
    C(0x00, &[0x81]),
    C(0xC5, &[0x66]),
    C(0x00, &[0xB6]),
    C(0xF5, &[0x06]),
    C(0x00, &[0xB1]),
    C(0xC6, &[0x06]),
    C(0x00, &[0x00]),
    C(0xFF, &[0xFF, 0xFF, 0xFF]),
    C(0x00, &[0x00]),
    C(0x00, &[0x00]),
    C(
        0xE1,
        &[
            0x00, 0x09, 0x0F, 0x0E, 0x07, 0x10, 0x0B, 0x0A, 0x04, 0x07, 0x0B, 0x08, 0x0F, 0x10,
            0x0A, 0x01,
        ],
    ),
    C(0x00, &[0x00]),
    C(
        0xE2,
        &[
            0x00, 0x09, 0x0F, 0x0E, 0x07, 0x10, 0x0B, 0x0A, 0x04, 0x07, 0x0B, 0x08, 0x0F, 0x10,
            0x0A, 0x01,
        ],
    ),
    C(0x11, &[0x00]),
    Pause(120),
    C(0x3A, &[0x55]),
    C(0x36, &[0x60]),
    C(0x2A, &[0x00, 0x00, 0x03, 0x1F]),
    C(0x2B, &[0x00, 0x00, 0x01, 0xDF]),
    C(0x51, &[0x7F]),
    C(0x53, &[0x2C]),
    C(0x55, &[0x02]),
    C(0x5E, &[0xFF]),
    C(0x29, &[0x00]),
    C(0x00, &[0x00]),
    C(0x2C, &[0x00]),
];

/// The NT35510's init: its vendor registers on pages 1 then 0, landscape, sleep out, RGB565,
/// brightness, display on, then memory write.
const NT35510_INIT: &[Step] = &[
    C(0xF0, &[0x55, 0xAA, 0x52, 0x08, 0x01]),
    C(0xB0, &[0x03, 0x03, 0x03]),
    C(0xB6, &[0x46, 0x46, 0x46]),
    C(0xB1, &[0x03, 0x03, 0x03]),
    C(0xB7, &[0x36, 0x36, 0x36]),
    C(0xB2, &[0x00, 0x00, 0x02]),
    C(0xB8, &[0x26, 0x26, 0x26]),
    C(0xBF, &[0x01]),
    C(0xB3, &[0x09, 0x09, 0x09]),
    C(0xB9, &[0x36, 0x36, 0x36]),
    C(0xB5, &[0x08, 0x08, 0x08]),
    C(0xBA, &[0x26, 0x26, 0x26]),
    C(0xBC, &[0x00, 0x80, 0x00]),
    C(0xBD, &[0x00, 0x80, 0x00]),
    C(0xBE, &[0x00, 0x50]),
    C(0xF0, &[0x55, 0xAA, 0x52, 0x08, 0x00]),
    C(0xB1, &[0xFC, 0x00]),
    C(0xB6, &[0x03]),
    C(0xB5, &[0x50]),
    C(0xB7, &[0x00, 0x00]),
    C(0xB8, &[0x01, 0x02, 0x02, 0x02]),
    C(0xBC, &[0x00, 0x00, 0x00]),
    C(0xCC, &[0x03, 0x00, 0x00]),
    C(0xBA, &[0x01]),
    // MADCTL is not taken without this pause
    Pause(200),
    C(0x36, &[0x60]),
    C(0x2A, &[0x00, 0x00, 0x03, 0x1F]),
    C(0x2B, &[0x00, 0x00, 0x01, 0xDF]),
    C(0x11, &[0x00]),
    Pause(20),
    C(0x3A, &[0x55]),
    C(0x51, &[0x7F]),
    C(0x53, &[0x2C]),
    C(0x55, &[0x02]),
    C(0x5E, &[0xFF]),
    C(0x29, &[0x00]),
    C(0x2C, &[0x00]),
];

pub struct Display {
    buffers: [&'static mut [u16]; 2],
    /// The buffer the LTDC shows.
    shown: usize,
    pub controller: Controller,
    _reset: Output<'static>,
    _backlight: Output<'static>,
}

/// A horizontal timing in lane byte clock cycles.
const fn lane_cycles(pixels: u16) -> u16 {
    (pixels as u32 * LANE_BYTE_KHZ / PIXEL_KHZ) as u16
}

/// Waits up to `limit` for `ready`.
async fn wait(limit: Duration, ready: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while !ready() {
        if Instant::now() > deadline {
            return false;
        }
        Timer::after_micros(100).await;
    }
    true
}

/// Spins up to [`DSI_TIMEOUT`] for `ready`.
fn spin(ready: impl Fn() -> bool) -> Result<(), Error> {
    let deadline = Instant::now() + DSI_TIMEOUT;
    while !ready() {
        if Instant::now() > deadline {
            return Err(Error::Timeout);
        }
    }
    Ok(())
}

/// A frame buffer's layout: aligned on the LTDC's 64-bit bursts.
fn frame_layout() -> Layout {
    Layout::from_size_align(PIXELS * 2, 8).unwrap_or_else(|_| defmt::unreachable!())
}

/// A frame buffer on the heap (in SDRAM), black.
fn frame_buffer() -> &'static mut [u16] {
    // SAFETY: the layout has a non-zero size; the allocation is freed only by `Display::drop`.
    let pixels = unsafe { alloc_zeroed(frame_layout()) } as *mut u16;
    if pixels.is_null() {
        defmt::panic!("display: no memory for a frame buffer");
    }
    // SAFETY: `pixels` is a fresh allocation of `PIXELS` zeroed u16s, owned from now on.
    unsafe { core::slice::from_raw_parts_mut(pixels, PIXELS) }
}

impl Display {
    /// Brings the display up: `reset` drives the panel's reset (PG3, active low), `backlight` its
    /// backlight (PJ12). The LTDC needs its pixel clock from PLL3's output R, 27.5 MHz.
    pub async fn new(
        mut reset: Output<'static>,
        mut backlight: Output<'static>,
        _ltdc: Peri<'static, peripherals::LTDC>,
        _dsi: Peri<'static, peripherals::DSIHOST>,
    ) -> Result<Self, Error> {
        reset.set_low();
        Timer::after_millis(20).await;
        reset.set_high();
        Timer::after_millis(10).await;
        backlight.set_high();

        rcc::enable_and_reset::<peripherals::LTDC>();
        rcc::enable_and_reset::<peripherals::DSIHOST>();

        // the DSI regulator, then the PLL: 25 MHz / 5 × 2 × 100 / 2 = 500 MHz on the lanes
        DSIHOST.wrpcr().write(|w| w.set_regen(true));
        if !wait(Duration::from_millis(100), || DSIHOST.wisr().read().rrs()).await {
            return Err(Error::Regulator);
        }
        DSIHOST.wrpcr().modify(|w| {
            w.set_ndiv(100);
            w.set_idf(5);
            w.set_odf(0);
        });
        DSIHOST.wrpcr().modify(|w| w.set_pllen(true));
        Timer::after_millis(1).await;
        if !wait(Duration::from_millis(100), || DSIHOST.wisr().read().pllls()).await {
            return Err(Error::Pll);
        }

        // the D-PHY: clock lane always in high speed, two data lanes, escape clock 62.5 / 4,
        // a bit of 2 ns (8 quarters of a nanosecond)
        DSIHOST.pctlr().write(|w| {
            w.set_cke(true);
            w.set_den(true);
        });
        DSIHOST.clcr().write(|w| w.set_dpcc(true));
        DSIHOST.pconfr().write(|w| w.set_nl(1));
        DSIHOST.ccr().write(|w| w.set_txeckdiv(4));
        DSIHOST.wpcr0().write(|w| w.set_uix4(8));
        DSIHOST.ier0().write_value(Default::default());
        DSIHOST.ier1().write_value(Default::default());

        // video burst mode from the LTDC, RGB565 (16-bit configuration 1), commands in low
        // power during the blankings
        DSIHOST.mcr().write(|w| w.set_cmdm(false));
        DSIHOST.wcfgr().write(|w| {
            w.set_dsim(false);
            w.set_colmux(0);
        });
        DSIHOST.vmcr().write(|w| {
            w.set_vmt(2);
            w.set_lpce(true);
            w.set_lphfpe(true);
            w.set_lphbpe(true);
            w.set_lpvae(true);
            w.set_lpvfpe(true);
            w.set_lpvbpe(true);
            w.set_lpvsae(true);
        });
        DSIHOST.vpcr().write(|w| w.set_vpsize(WIDTH as u16));
        DSIHOST.vccr().write(|w| w.set_numc(0));
        DSIHOST.vnpcr().write(|w| w.set_npsize(0xFFF));
        DSIHOST.lvcidr().write(|w| w.set_vcid(0));
        DSIHOST.lpcr().write(|_| {});
        DSIHOST.lcolcr().write(|w| w.set_colc(0));
        DSIHOST.vhsacr().write(|w| w.set_hsa(lane_cycles(HSYNC)));
        DSIHOST.vhbpcr().write(|w| w.set_hbp(lane_cycles(HBP)));
        DSIHOST
            .vlcr()
            .write(|w| w.set_hline(lane_cycles(WIDTH as u16 + HSYNC + HBP + HFP)));
        DSIHOST.vvsacr().write(|w| w.set_vsa(VSYNC));
        DSIHOST.vvbpcr().write(|w| w.set_vbp(VBP));
        DSIHOST.vvfpcr().write(|w| w.set_vfp(VFP));
        DSIHOST.vvacr().write(|w| w.set_va(HEIGHT as u16));
        DSIHOST.lpmcr().write(|w| {
            w.set_lpsize(64);
            w.set_vlpsize(64);
        });

        let buffers = [frame_buffer(), frame_buffer()];

        // the LTDC: timings, a black background, layer 1 full screen in RGB565
        LTDC.gcr().write(|w| {
            w.set_hspol(Hspol::ACTIVE_LOW);
            w.set_vspol(Vspol::ACTIVE_LOW);
            w.set_depol(Depol::ACTIVE_LOW);
            w.set_pcpol(Pcpol::RISING_EDGE);
        });
        LTDC.sscr().write(|w| {
            w.set_hsw(HSYNC - 1);
            w.set_vsh(VSYNC - 1);
        });
        let (ahbp, avbp) = (HSYNC + HBP - 1, VSYNC + VBP - 1);
        LTDC.bpcr().write(|w| {
            w.set_ahbp(ahbp);
            w.set_avbp(avbp);
        });
        LTDC.awcr().write(|w| {
            w.set_aaw(ahbp + WIDTH as u16);
            w.set_aah(avbp + HEIGHT as u16);
        });
        LTDC.twcr().write(|w| {
            w.set_totalw(ahbp + WIDTH as u16 + HFP);
            w.set_totalh(avbp + HEIGHT as u16 + VFP);
        });
        LTDC.bccr().write(|_| {});
        let layer = LTDC.layer(0);
        layer.whpcr().write(|w| {
            w.set_whstpos(ahbp + 1);
            w.set_whsppos(ahbp + WIDTH as u16);
        });
        layer.wvpcr().write(|w| {
            w.set_wvstpos(avbp + 1);
            w.set_wvsppos(avbp + HEIGHT as u16);
        });
        layer.pfcr().write(|w| w.set_pf(Pf::RGB565));
        layer.cacr().write(|w| w.set_consta(0xFF));
        layer.dccr().write(|_| {});
        layer.bfcr().write(|w| {
            w.set_bf1(Bf1::PIXEL);
            w.set_bf2(Bf2::PIXEL);
        });
        layer
            .cfbar()
            .write(|w| w.set_cfbadd(buffers[0].as_ptr() as u32));
        layer.cfblr().write(|w| {
            w.set_cfbp((WIDTH * 2) as u16);
            w.set_cfbll((WIDTH * 2 + 7) as u16);
        });
        layer.cfblnr().write(|w| w.set_cfblnbr(HEIGHT as u16));
        layer.cr().write(|w| w.set_len(true));
        LTDC.srcr().write(|w| w.set_imr(Imr::RELOAD));
        LTDC.gcr().modify(|w| w.set_ltdcen(true));

        // the DSI host after the LTDC, and bus turn-arounds for the panel's answers
        DSIHOST.cr().modify(|w| w.set_en(true));
        DSIHOST.wcr().modify(|w| w.set_dsien(true));
        DSIHOST.pcr().write(|w| w.set_btae(true));

        let mut display = Self {
            buffers,
            shown: 0,
            controller: Controller::Otm8009a,
            _reset: reset,
            _backlight: backlight,
        };
        let id2 = display.read(RDID2)?;
        let (controller, init) = if id2 == NT35510_ID2 {
            (Controller::Nt35510, NT35510_INIT)
        } else {
            match display.read(RDID1)? {
                OTM8009A_ID1 => (Controller::Otm8009a, OTM8009A_INIT),
                id1 => return Err(Error::UnknownPanel { id1, id2 }),
            }
        };
        display.controller = controller;
        for step in init {
            match step {
                Step::Command(command, parameters) => display.command(*command, parameters)?,
                Step::Pause(ms) => Timer::after_millis(*ms).await,
            }
        }
        Ok(display)
    }

    /// Sends a short packet.
    fn short(&self, data_type: u8, data0: u8, data1: u8) -> Result<(), Error> {
        spin(|| DSIHOST.gpsr().read().cmdfe())?;
        DSIHOST.ghcr().write(|w| {
            w.set_dt(data_type);
            w.set_vcid(0);
            w.set_wclsb(data0);
            w.set_wcmsb(data1);
        });
        Ok(())
    }

    /// Sends a DCS command with its parameters.
    fn command(&self, command: u8, parameters: &[u8]) -> Result<(), Error> {
        match parameters {
            [] => self.short(DCS_SHORT_WRITE_0, command, 0),
            [p] => self.short(DCS_SHORT_WRITE_1, command, *p),
            _ => {
                spin(|| DSIHOST.gpsr().read().cmdfe())?;
                let count = parameters.len() + 1;
                let bytes = core::iter::once(&command).chain(parameters);
                let mut word = [0u8; 4];
                for (i, b) in bytes.enumerate() {
                    word[i % 4] = *b;
                    if i % 4 == 3 || i + 1 == count {
                        spin(|| !DSIHOST.gpsr().read().pwrff())?;
                        DSIHOST
                            .gpdr()
                            .write_value(embassy_stm32::pac::dsihost::regs::Gpdr(
                                u32::from_le_bytes(word),
                            ));
                        word = [0; 4];
                    }
                }
                DSIHOST.ghcr().write(|w| {
                    w.set_dt(DCS_LONG_WRITE);
                    w.set_vcid(0);
                    w.set_wclsb(count as u8);
                    w.set_wcmsb((count >> 8) as u8);
                });
                Ok(())
            }
        }
    }

    /// Reads the one byte a DCS command answers.
    fn read(&self, command: u8) -> Result<u8, Error> {
        self.short(DCS_READ, command, 0)?;
        spin(|| {
            let gpsr = DSIHOST.gpsr().read();
            !gpsr.rcb() && !gpsr.prdfe()
        })?;
        Ok(DSIHOST.gpdr().read().data1())
    }

    /// The two frame buffers, `WIDTH * HEIGHT` pixels each, for the program's life. The LTDC
    /// reads the one [`Display::shown`] gives; a frame is drawn in the other.
    pub fn buffers(&mut self) -> [*mut u16; 2] {
        let [a, b] = &mut self.buffers;
        [a.as_mut_ptr(), b.as_mut_ptr()]
    }

    /// The buffer the LTDC shows: 0 or 1.
    pub fn shown(&self) -> usize {
        self.shown
    }

    /// Shows the frame buffer drawn in, from the next vertical blanking; returns once it shows.
    pub async fn swap(&mut self) {
        let back = 1 - self.shown;
        LTDC.layer(0)
            .cfbar()
            .write(|w| w.set_cfbadd(self.buffers[back].as_ptr() as u32));
        LTDC.srcr().write(|w| w.set_vbr(Vbr::RELOAD));
        // a frame lasts 28.5 ms
        wait(Duration::from_millis(100), || {
            LTDC.srcr().read().vbr() == Vbr::NO_EFFECT
        })
        .await;
        self.shown = back;
    }
}

/// A display that failed to start: the LTDC and the DSI host stop, the frame buffers go back to
/// the heap.
impl Drop for Display {
    fn drop(&mut self) {
        LTDC.gcr().modify(|w| w.set_ltdcen(false));
        DSIHOST.wcr().modify(|w| w.set_dsien(false));
        DSIHOST.cr().modify(|w| w.set_en(false));
        for buffer in &mut self.buffers {
            // SAFETY: each buffer came from `frame_buffer`, with `frame_layout`, and the LTDC
            // no longer reads it.
            unsafe { dealloc(buffer.as_mut_ptr() as *mut u8, frame_layout()) };
        }
    }
}
