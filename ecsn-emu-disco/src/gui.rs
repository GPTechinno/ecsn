//! The window (BLIT): the board's display at its size, under it the board
//! with its four user LEDs, its network address, the chain's validated tip,
//! the emulation's pace and a RESET button; the firmware's logs beside them.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use blit::{Animation, AnimationOptions, Sides};
use blit_desktop::{Application, Config, DesktopContext, EventLoopProxy, Scope, cpu};
use blit_gui::clip::BoundsClip;
use blit_gui::color::Color;
use blit_gui::image::{Image, ImageData, ImageFormat, ImageHandle, ImagePixels, ImageSampling};
use blit_gui::rectangle::Rectangle;
use blit_gui::style::{Border, BorderRadius};
use blit_gui::text::system::{FontStack, TextConfig, TextSystem};
use blit_gui::text::{FontId, Text, TextStyle};
use blit_gui_widgets::interact;
use blit_gui_widgets::layout::{Align, Sizing, flex};
use blit_gui_widgets::scroll::ScrollState;
use blit_gui_widgets::scroll::uniform_list::UniformList;
use blit_text::{Family, FontData};

use crate::board::SCREEN;
use crate::panel::Controller;
use crate::session::{Command, Session, Snapshot};

type Ctx = DesktopContext<cpu::Backend>;
type Ui<'a> = blit::Ui<'a, Ctx>;

static FONT: &[u8] = include_bytes!(env!("ECSN_EMU_FONT"));
static FONT_MONO: &[u8] = include_bytes!(env!("ECSN_EMU_FONT_MONO"));
const MONO: FontId = FontId(1);

/// The frame around the display.
const BEZEL: f32 = 16.0;
const BOARD_WIDTH: f32 = SCREEN.0 as f32 + 2.0 * BEZEL;
const SIDE_WIDTH: f32 = 560.0;
const GAP: f32 = 18.0;
const LOG_LINE: f32 = 16.0;
const LED_SIZE: f32 = 30.0;

const BACKGROUND: Color = Color::rgb(0x12, 0x14, 0x18);
const PANEL: Color = Color::rgb(0x1c, 0x1f, 0x26);
const BOARD: Color = Color::rgb(0x0e, 0x3b, 0x2c);
const GLASS: Color = Color::rgb(0x05, 0x05, 0x06);
const TEXT: Color = Color::rgb(0xe6, 0xe8, 0xee);
const MUTED: Color = Color::rgb(0x9a, 0xa3, 0xb3);
const ACCENT: Color = Color::rgb(0xf2, 0xc8, 0x1e);
const HOVER: Color = Color::rgba(0xff, 0xff, 0xff, 0x28);
const PRESSED: Color = Color::rgba(0xf2, 0xc8, 0x1e, 0x60);
/// LD1 to LD4: green, orange, red, blue.
const LEDS: [(&str, Color); 4] = [
    ("LD1", Color::rgb(0x3d, 0xf5, 0x6b)),
    ("LD2", Color::rgb(0xff, 0x9f, 0x1c)),
    ("LD3", Color::rgb(0xff, 0x40, 0x40)),
    ("LD4", Color::rgb(0x40, 0x8c, 0xff)),
];
const LED_OFF: Color = Color::rgb(0x2a, 0x2e, 0x2a);

/// Wakes the window after each snapshot.
pub struct Wake;

/// What `run` hands to the window.
struct Launch {
    elf: Vec<u8>,
    qspi: Option<PathBuf>,
    panel: Controller,
}

static HANDOFF: Mutex<Option<Launch>> = Mutex::new(None);
static SESSION: Mutex<Option<Session>> = Mutex::new(None);

struct App {
    snapshot: Arc<Mutex<Snapshot>>,
    logs_scroll: ScrollState,
    seen_logs: u64,
    /// The display's picture, and the screen generation it shows.
    screen: Option<(u64, ImageHandle)>,
}

fn text(ui: Ui<'_>, text: &str, size: f32, color: Color) {
    let mut ui = ui;
    ui.insert(Text::new(text).style(TextStyle::new().size(size).color(color)));
}

/// A button: its label in a frame; whether it was clicked.
fn button(ui: Ui<'_>, label: &str) -> bool {
    let mut b = ui
        .layout(flex::column())
        .pad(Sides::new().top(6.0).right(14.0).bottom(6.0).left(14.0));
    let press = b.interact(interact::press());
    b.insert(
        Rectangle::new()
            .background(if press.active {
                PRESSED
            } else if press.hovered {
                HOVER
            } else {
                BACKGROUND
            })
            .border(Border::solid(1.5, ACCENT))
            .radius(BorderRadius::uniform(6.0)),
    );
    b.child()
        .insert(Text::new(label).style(TextStyle::new().size(14.0).color(TEXT)));
    press.clicked
}

impl Application for App {
    type Input = Wake;
    type Graphics = cpu::Backend;

    fn new(proxy: EventLoopProxy<Wake>, _: Scope<Self>, _: &mut Ctx) -> Self {
        let launch = HANDOFF.lock().unwrap().take().expect("emulator handoff");
        let session = Session::start(launch.elf, launch.qspi, launch.panel, move || {
            let _ = proxy.send_event(Wake);
        });
        let snapshot = session.snapshot.clone();
        *SESSION.lock().unwrap() = Some(session);
        Self {
            snapshot,
            logs_scroll: ScrollState::default(),
            seen_logs: 0,
            screen: None,
        }
    }

    fn input(&mut self, _: Wake) {}

    fn build(&mut self, ui: Ui<'_>) {
        let snap = self.snapshot.lock().unwrap().clone();
        let mut root = ui.layout(flex::row().gap(GAP)).pad(GAP);
        root.insert(Rectangle::new().background(BACKGROUND));
        let reset;

        // The display, then the board.
        {
            let mut left = root
                .child()
                .item(flex::item().width(Sizing::fixed(BOARD_WIDTH)))
                .layout(flex::column().gap(14.0));
            {
                let mut bezel = left.child().layout(flex::column()).pad(BEZEL);
                bezel.insert(
                    Rectangle::new()
                        .background(GLASS)
                        .border(Border::solid(2.0, Color::rgb(0x2a, 0x2e, 0x36)))
                        .radius(BorderRadius::uniform(10.0)),
                );
                let screen = &snap.screen;
                if !screen.frame.rgba.is_empty()
                    && self
                        .screen
                        .as_ref()
                        .is_none_or(|(generation, _)| *generation != screen.generation)
                {
                    let image = bezel
                        .context_mut()
                        .create_image(ImageData::new(
                            ImagePixels::Owned(screen.frame.rgba.clone().into_boxed_slice()),
                            ImageFormat::Rgba8,
                            screen.frame.width,
                            screen.frame.height,
                        ))
                        .expect("display image");
                    self.screen = Some((screen.generation, image));
                }
                let mut glass = bezel.child().item(
                    flex::item()
                        .width(Sizing::fixed(SCREEN.0 as f32))
                        .height(Sizing::fixed(SCREEN.1 as f32)),
                );
                match &self.screen {
                    Some((_, image)) => {
                        glass.insert(Image::new(image).sampling(ImageSampling::Nearest))
                    }
                    None => glass.insert(Rectangle::new().background(Color::rgb(0, 0, 0))),
                }
            }
            let mut board = left
                .child()
                .item(flex::item().width(Sizing::fixed(BOARD_WIDTH)))
                .layout(flex::row().gap(28.0).align(Align::Center))
                .pad(16.0);
            board.insert(
                Rectangle::new()
                    .background(BOARD)
                    .border(Border::solid(2.0, Color::rgb(0x1f, 0x5e, 0x47)))
                    .radius(BorderRadius::uniform(10.0)),
            );
            {
                let mut about = board.child().layout(flex::column().gap(6.0));
                about
                    .child()
                    .build(|ui: Ui<'_>| text(ui, "STM32H747I-DISCO", 20.0, ACCENT));
                for line in [
                    "STM32H747XI (Cortex-M7 at 400 MHz)",
                    "32 MB SDRAM, 2 × MT25QL512 QSPI, LAN8742A",
                ] {
                    about
                        .child()
                        .build(|ui: Ui<'_>| text(ui, line, 12.0, MUTED));
                }
                let pace = format!(
                    "{:.1} s virtual, ×{:.2} real time, {:.0} MIPS, asleep {:.0} %",
                    snap.virtual_s,
                    snap.speed,
                    snap.mips,
                    100.0 * snap.asleep
                );
                about
                    .child()
                    .build(|ui: Ui<'_>| text(ui, &pace, 12.0, MUTED));
                if let Some(stopped) = &snap.stopped {
                    about
                        .child()
                        .build(|ui: Ui<'_>| text(ui, stopped, 13.0, Color::rgb(0xff, 0x6b, 0x6b)));
                }
            }
            {
                let mut leds = board.child().layout(flex::row().gap(22.0));
                for (i, (label, lit)) in LEDS.iter().enumerate() {
                    let mut led = leds
                        .child()
                        .layout(flex::column().gap(4.0).align(Align::Center));
                    led.child()
                        .item(
                            flex::item()
                                .width(Sizing::fixed(LED_SIZE))
                                .height(Sizing::fixed(LED_SIZE)),
                        )
                        .insert(
                            Rectangle::new()
                                .background(if snap.leds[i] { *lit } else { LED_OFF })
                                .border(Border::solid(1.0, Color::rgb(0x0a, 0x0a, 0x0a)))
                                .radius(BorderRadius::uniform(LED_SIZE / 2.0)),
                        );
                    led.child().build(|ui: Ui<'_>| text(ui, label, 12.0, TEXT));
                }
            }
            let mut state = board.child().layout(flex::column().gap(8.0));
            let address = snap.address.as_deref().unwrap_or("waiting for DHCP");
            state
                .child()
                .build(|ui: Ui<'_>| text(ui, &format!("Ethernet: {address}"), 14.0, TEXT));
            let validated = snap.validated.as_deref().unwrap_or("-");
            state
                .child()
                .build(|ui: Ui<'_>| text(ui, &format!("Validated up to: {validated}"), 14.0, TEXT));
            reset = state.child().build(|ui: Ui<'_>| button(ui, "RESET"));
        }

        // The logs.
        {
            let mut logs = root
                .child()
                .item(flex::item().width(Sizing::fixed(SIDE_WIDTH)).grow())
                .layout(flex::column().gap(4.0))
                .pad(12.0);
            logs.insert(
                Rectangle::new()
                    .background(PANEL)
                    .radius(BorderRadius::uniform(8.0)),
            );
            logs.child()
                .build(|ui: Ui<'_>| text(ui, "Firmware logs", 14.0, ACCENT));
            // follow new lines while the view is at the end
            let scroll = &mut self.logs_scroll;
            if snap.log_count != self.seen_logs
                && scroll.offset >= scroll.maximum_offset() - LOG_LINE / 2.0
            {
                scroll.scroll_to(f32::MAX, Animation::default(), AnimationOptions::new());
            }
            self.seen_logs = snap.log_count;
            logs.child().item(flex::item().grow()).build(
                UniformList::new(scroll, snap.logs.iter())
                    .item_extent(LOG_LINE)
                    .scroll_speed(2.0)
                    .scrollbar_thickness(6.0)
                    .minimum_thumb_extent(24.0)
                    .clip(BoundsClip)
                    .build_scrollbar(
                        |mut ui: Ui<'_>, line: &String| {
                            let color = if line.contains("ERROR") {
                                Color::rgb(0xff, 0x6b, 0x6b)
                            } else if line.contains("WARN") {
                                Color::rgb(0xff, 0xc1, 0x4d)
                            } else if line.contains(" emu: ") || line.contains(" net: ") {
                                MUTED
                            } else {
                                TEXT
                            };
                            let short: String = line.chars().take(300).collect();
                            ui.insert(
                                Text::new(&short)
                                    .style(TextStyle::new().font(MONO).size(12.0).color(color)),
                            );
                        },
                        |mut ui: Ui<'_>| {
                            ui.insert(
                                Rectangle::new()
                                    .background(Color::rgb(0x1a, 0x1c, 0x22))
                                    .radius(BorderRadius::uniform(3.0)),
                            );
                        },
                        |mut ui: Ui<'_>, active: bool| {
                            ui.insert(
                                Rectangle::new()
                                    .background(if active { ACCENT } else { MUTED })
                                    .radius(BorderRadius::uniform(3.0)),
                            );
                        },
                    ),
            );
        }
        if reset && let Some(session) = SESSION.lock().unwrap().as_ref() {
            session.send(Command::Reset);
        }
    }
}

/// Opens the window with the board running `elf`, and stops the board, which
/// saves its flash image, when the window closes.
pub fn run(elf: Vec<u8>, qspi: Option<PathBuf>, panel: Controller) {
    let text = TextSystem::new(
        TextConfig {
            fonts: vec![
                FontStack {
                    id: FontId::default(),
                    families: vec![Family::embedded([FontData::Shared(FONT.to_vec().into())])],
                },
                FontStack {
                    id: MONO,
                    families: vec![Family::embedded([FontData::Shared(
                        FONT_MONO.to_vec().into(),
                    )])],
                },
            ],
            text_cache_capacity: 1024 * 1024,
            layout_cache_capacity: 2 * 1024 * 1024,
        },
        blit_text_parley::Backend::new(),
    )
    .expect("text system");
    let graphics = cpu::Backend::new(text, cpu::Config::default());
    *HANDOFF.lock().unwrap() = Some(Launch { elf, qspi, panel });
    blit_desktop::run::<App>(Config {
        title: "STM32H747I-DISCO emulator".into(),
        width: (BOARD_WIDTH + SIDE_WIDTH + 3.0 * GAP) as u32,
        height: 730,
        graphics,
    })
    .expect("window");
    if let Some(session) = SESSION.lock().unwrap().take() {
        session.stop();
    }
}
