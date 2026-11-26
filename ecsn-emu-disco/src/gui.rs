//! The window (BLIT): the board, its front photographed, with what its
//! display shows in the display's place and its four user LEDs shining where
//! they are lit (or the display alone at its size); under it what the LEDs
//! mean, its network address, the chain's validated tip, the emulation's pace
//! and the RESET button; the firmware's logs beside them, scrolled both ways.

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use blit::{
    Animation, AnimationOptions, Axis, Constraints, NodeTarget, Place, PlaceCx, Point, Rect, Sides,
    Size,
};
use blit_desktop::{Application, Config, DesktopContext, EventLoopProxy, Scope, cpu};
use blit_gui::clip::BoundsClip;
use blit_gui::color::Color;
use blit_gui::image::{Image, ImageData, ImageFormat, ImageHandle, ImagePixels, ImageSampling};
use blit_gui::rectangle::Rectangle;
use blit_gui::shadow::Shadow;
use blit_gui::style::{Border, BorderRadius};
use blit_gui::text::system::{FontStack, TextConfig, TextSystem};
use blit_gui::text::{FontId, Text, TextStyle};
use blit_gui_widgets::interact;
use blit_gui_widgets::layout::{Align, Sizing, flex};
use blit_gui_widgets::scroll::ScrollState;
use blit_gui_widgets::scroll::area::ScrollArea;
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

/// The frame around the display, shown alone.
const BEZEL: f32 = 16.0;
const BOARD_WIDTH: f32 = SCREEN.0 as f32 + 2.0 * BEZEL;
/// The board's front, from ST's product page (`assets/stm32h747i-disco.avif`,
/// as PNG), and where the display's active area is in it.
static PHOTO: &[u8] = include_bytes!("../assets/stm32h747i-disco.png");
const PHOTO_SIZE: (f32, f32) = (1000.0, 743.0);
const PHOTO_SCREEN: (f32, f32, f32) = (202.0, 83.0, 611.0);
/// LD1 to LD4 in the photograph, top to bottom in the column left of the RESET
/// button: the centre of each lens, and the lens's size.
const PHOTO_LEDS: [(f32, f32); 4] = [(88.0, 553.6), (88.0, 566.2), (88.0, 580.1), (88.0, 593.6)];
const PHOTO_LED: (f32, f32) = (13.0, 6.0);
const SIDE_WIDTH: f32 = 560.0;
const GAP: f32 = 18.0;
const LOG_LINE: f32 = 16.0;
/// The scrollbars' thickness.
const BAR: f32 = 6.0;
/// The legend's colour key.
const KEY_SIZE: f32 = 10.0;

const BACKGROUND: Color = Color::rgb(0x12, 0x14, 0x18);
const PANEL: Color = Color::rgb(0x1c, 0x1f, 0x26);
const BOARD: Color = Color::rgb(0x0e, 0x3b, 0x2c);
const GLASS: Color = Color::rgb(0x05, 0x05, 0x06);
const TEXT: Color = Color::rgb(0xe6, 0xe8, 0xee);
const MUTED: Color = Color::rgb(0x9a, 0xa3, 0xb3);
const ACCENT: Color = Color::rgb(0xf2, 0xc8, 0x1e);
const HOVER: Color = Color::rgba(0xff, 0xff, 0xff, 0x28);
const PRESSED: Color = Color::rgba(0xf2, 0xc8, 0x1e, 0x60);
/// LD1 to LD4: their colour (green, orange, red, blue) and what the ECSN
/// firmware shows with them.
const LEDS: [(&str, [u8; 3], &str); 4] = [
    ("LD1", [0x3d, 0xf5, 0x6b], "firmware running (blinks)"),
    ("LD2", [0xff, 0x9f, 0x1c], "connected to the peer"),
    ("LD3", [0xff, 0x40, 0x40], "not used"),
    ("LD4", [0x40, 0x8c, 0xff], "not used"),
];
const LED_OFF: Color = Color::rgb(0x2a, 0x2e, 0x2a);

/// Places a node at `x`, `y` in its parent, out of the parent's flow, with a
/// fixed size.
struct At {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl<C: blit::Context<Scalar = f32>> Place<C> for At {
    fn place(&self, cx: &mut PlaceCx<'_, C>, target: Rect<f32>) -> Point<f32> {
        cx.resolve(Constraints::tight(Size::new(self.width, self.height)));
        Point::new(target.x + self.x, target.y + self.y)
    }
}

/// Places a node its parent's origin moved left by this much, at its own
/// size: a log line scrolled sideways.
struct Shift(f32);

impl<C: blit::Context<Scalar = f32>> Place<C> for Shift {
    fn place(&self, cx: &mut PlaceCx<'_, C>, target: Rect<f32>) -> Point<f32> {
        cx.resolve(Constraints::loose(Size::new(f32::INFINITY, f32::INFINITY)));
        Point::new(target.x - self.0, target.y)
    }
}

/// The sideways part of a scroll: across, or down for a wheel that only turns
/// (with Shift, or over the sideways bar).
fn sideways_delta(delta: Point<f32>) -> f32 {
    if delta.x != 0.0 { delta.x } else { delta.y }
}

/// A lit LED on the photograph, centred on `centre`: a wide soft halo of its
/// colour, a tighter bright one, and the lens itself almost white.
fn glow<L: blit::Layout<Ctx>>(
    board: &mut blit::Ui<'_, Ctx, blit::state::Open<L>>,
    centre: (f32, f32),
    [r, g, b]: [u8; 3],
) {
    let (width, height) = PHOTO_LED;
    let at = || At {
        x: centre.0 - width / 2.0,
        y: centre.1 - height / 2.0,
        width,
        height,
    };
    board.child().place(NodeTarget::Parent, at()).insert(
        Shadow::new(Color::rgba(r, g, b, 0xb0))
            .blur(16.0)
            .spread(6.0)
            .radius(BorderRadius::uniform(height)),
    );
    board.child().place(NodeTarget::Parent, at()).insert(
        Shadow::new(Color::rgba(r, g, b, 0xe0))
            .blur(5.0)
            .spread(1.5)
            .radius(BorderRadius::uniform(height / 2.0)),
    );
    let hot = |c: u8| c + ((255 - c) as f32 * 0.75) as u8;
    board.child().place(NodeTarget::Parent, at()).insert(
        Rectangle::new()
            .background(Color::rgb(hot(r), hot(g), hot(b)))
            .radius(BorderRadius::uniform(height / 2.0)),
    );
}

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
    /// The logs' sideways scroll, the same for every line.
    logs_sideways: ScrollState,
    seen_logs: u64,
    /// The display's picture, and the screen generation it shows.
    screen: Option<(u64, ImageHandle)>,
    /// The board's photograph, if it decodes.
    photo: Option<ImageHandle>,
    /// Show the display alone, at its size, instead of on the board.
    actual_size: bool,
}

/// The photograph, RGBA.
fn decode_photo() -> Result<(u32, u32, Vec<u8>), png::DecodingError> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(PHOTO));
    decoder.set_transformations(png::Transformations::ALPHA);
    let mut reader = decoder.read_info()?;
    let mut rgba = vec![0; reader.output_buffer_size().unwrap_or(0)];
    let info = reader.next_frame(&mut rgba)?;
    rgba.truncate(info.buffer_size());
    Ok((info.width, info.height, rgba))
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

    fn new(proxy: EventLoopProxy<Wake>, _: Scope<Self>, ctx: &mut Ctx) -> Self {
        let photo = match decode_photo() {
            Ok((width, height, rgba)) => ctx
                .create_image(ImageData::new(
                    ImagePixels::Owned(rgba.into_boxed_slice()),
                    ImageFormat::Rgba8,
                    width,
                    height,
                ))
                .ok(),
            Err(e) => {
                eprintln!("ecsn-emu-disco: board photo not shown: {e}");
                None
            }
        };
        let launch = HANDOFF.lock().unwrap().take().expect("emulator handoff");
        let session = Session::start(launch.elf, launch.qspi, launch.panel, move || {
            let _ = proxy.send_event(Wake);
        });
        let snapshot = session.snapshot.clone();
        *SESSION.lock().unwrap() = Some(session);
        Self {
            snapshot,
            logs_scroll: ScrollState::default(),
            logs_sideways: ScrollState::default(),
            seen_logs: 0,
            screen: None,
            photo,
            actual_size: false,
        }
    }

    fn input(&mut self, _: Wake) {}

    fn build(&mut self, ui: Ui<'_>) {
        let snap = self.snapshot.lock().unwrap().clone();
        let mut root = ui.layout(flex::row().gap(GAP)).pad(GAP);
        root.insert(Rectangle::new().background(BACKGROUND));
        let reset;
        let mut toggle = false;

        // The display on the board's photograph, or alone; then the board's
        // state.
        {
            let on_board = self.photo.is_some() && !self.actual_size;
            let width = if on_board { PHOTO_SIZE.0 } else { BOARD_WIDTH };
            let mut left = root
                .child()
                .item(flex::item().width(Sizing::fixed(width)))
                .layout(flex::column().gap(14.0));
            let screen = &snap.screen;
            if !screen.frame.rgba.is_empty()
                && self
                    .screen
                    .as_ref()
                    .is_none_or(|(generation, _)| *generation != screen.generation)
            {
                let image = left
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
            if let Some(photo) = self.photo.as_ref().filter(|_| on_board) {
                // The picture fills the display's active area, scaled down to
                // the photograph's.
                let (x, y, w) = PHOTO_SCREEN;
                let h = w * SCREEN.1 as f32 / SCREEN.0 as f32;
                let mut board = left
                    .child()
                    .item(
                        flex::item()
                            .width(Sizing::fixed(PHOTO_SIZE.0))
                            .height(Sizing::fixed(PHOTO_SIZE.1)),
                    )
                    .layout(flex::column())
                    .pad(Sides::new().left(x).top(y));
                board.insert(Image::new(photo).sampling(ImageSampling::Bilinear));
                {
                    let mut glass = board.child().item(
                        flex::item()
                            .width(Sizing::fixed(w))
                            .height(Sizing::fixed(h)),
                    );
                    match &self.screen {
                        Some((_, image)) => {
                            glass.insert(Image::new(image).sampling(ImageSampling::Bilinear))
                        }
                        None => glass.insert(Rectangle::new().background(Color::rgb(0, 0, 0))),
                    }
                }
                for ((_, rgb, _), (&centre, _)) in LEDS
                    .iter()
                    .zip(PHOTO_LEDS.iter().zip(snap.leds))
                    .filter(|(_, (_, lit))| *lit)
                {
                    glow(&mut board, centre, *rgb);
                }
            } else {
                let mut bezel = left.child().layout(flex::column()).pad(BEZEL);
                bezel.insert(
                    Rectangle::new()
                        .background(GLASS)
                        .border(Border::solid(2.0, Color::rgb(0x2a, 0x2e, 0x36)))
                        .radius(BorderRadius::uniform(10.0)),
                );
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
                .item(flex::item().width(Sizing::fixed(width)))
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
                // What the LEDs mean, each with its colour; the photograph
                // shows them lit, and without it the key does.
                let mut legend = board.child().layout(flex::column().gap(5.0));
                for ((label, [r, g, b], meaning), lit) in LEDS.iter().zip(snap.leds) {
                    let mut row = legend
                        .child()
                        .layout(flex::row().gap(8.0).align(Align::Center));
                    let key = if on_board || lit {
                        Color::rgb(*r, *g, *b)
                    } else {
                        LED_OFF
                    };
                    row.child()
                        .item(
                            flex::item()
                                .width(Sizing::fixed(KEY_SIZE))
                                .height(Sizing::fixed(KEY_SIZE)),
                        )
                        .insert(
                            Rectangle::new()
                                .background(key)
                                .radius(BorderRadius::uniform(KEY_SIZE / 2.0)),
                        );
                    row.child().build(|ui: Ui<'_>| text(ui, label, 12.0, TEXT));
                    row.child()
                        .build(|ui: Ui<'_>| text(ui, meaning, 12.0, MUTED));
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
            let mut buttons = state.child().layout(flex::row().gap(10.0));
            reset = buttons.child().build(|ui: Ui<'_>| button(ui, "RESET"));
            if self.photo.is_some() {
                let label = if on_board { "DISPLAY 1:1" } else { "BOARD" };
                toggle = buttons.child().build(|ui: Ui<'_>| button(ui, label));
            }
        }
        if toggle {
            self.actual_size = !self.actual_size;
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
            // Sideways, every line moves by the same offset: the bar under
            // the list (its thumb, its track, the wheel over it) moves it, and
            // so does a sideways scroll, or the wheel with Shift, over a line.
            let sideways = &mut self.logs_sideways;
            let shift = sideways.offset.clamp(0.0, sideways.maximum_offset());
            let overflow = sideways.content_extent > sideways.viewport_extent + 0.5;
            let swiped = Cell::new(0.0f32);
            logs.child().item(flex::item().grow()).build(
                UniformList::new(scroll, snap.logs.iter())
                    .item_extent(LOG_LINE)
                    .scroll_speed(2.0)
                    .scrollbar_thickness(BAR)
                    .minimum_thumb_extent(24.0)
                    .clip(BoundsClip)
                    .build_scrollbar(
                        |ui: Ui<'_>, line: &String| {
                            let mut ui = ui.layout(flex::row());
                            let shifted = matches!(ui.input(), blit::Input::Scroll { modifiers, .. } if modifiers.shift());
                            let swipe = if shifted {
                                ui.interact(interact::scroll())
                            } else {
                                ui.interact(interact::scroll().axis(Axis::Horizontal))
                            };
                            swiped.set(swiped.get() + sideways_delta(swipe.delta));
                            let color = if line.contains("ERROR") {
                                Color::rgb(0xff, 0x6b, 0x6b)
                            } else if line.contains("WARN") {
                                Color::rgb(0xff, 0xc1, 0x4d)
                            } else if line.contains(" emu: ") || line.contains(" net: ") {
                                MUTED
                            } else {
                                TEXT
                            };
                            ui.child().place(NodeTarget::Parent, Shift(shift)).insert(
                                Text::new(line)
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
            // The bar's content is the longest line, invisible: its width is
            // what there is to scroll.
            let longest = snap
                .logs
                .iter()
                .max_by_key(|line| line.chars().count())
                .map_or("", String::as_str);
            let sideways = &mut self.logs_sideways;
            let mut bar = logs
                .child()
                .item(flex::item().height(Sizing::fixed(BAR + 2.0)))
                .layout(flex::column());
            let wheel = bar.interact(interact::scroll());
            swiped.set(swiped.get() + sideways_delta(wheel.delta));
            bar.child().build(
                ScrollArea::new(sideways)
                    .axis(Axis::Horizontal)
                    .scroll_speed(2.0)
                    .scrollbar_thickness(BAR)
                    .minimum_thumb_extent(24.0)
                    .build_scrollbar(
                        |mut ui: Ui<'_>| {
                            ui.insert(
                                Text::new(longest).style(
                                    TextStyle::new()
                                        .font(MONO)
                                        .size(12.0)
                                        .color(Color::rgba(0, 0, 0, 0)),
                                ),
                            );
                        },
                        |mut ui: Ui<'_>| {
                            if overflow {
                                ui.insert(
                                    Rectangle::new()
                                        .background(Color::rgb(0x1a, 0x1c, 0x22))
                                        .radius(BorderRadius::uniform(3.0)),
                                );
                            }
                        },
                        |mut ui: Ui<'_>, active: bool| {
                            if overflow {
                                ui.insert(
                                    Rectangle::new()
                                        .background(if active { ACCENT } else { MUTED })
                                        .radius(BorderRadius::uniform(3.0)),
                                );
                            }
                        },
                    ),
            );
            drop(bar);
            let swiped = swiped.get();
            if swiped != 0.0 {
                sideways.offset =
                    (sideways.offset + 2.0 * swiped).clamp(0.0, sideways.maximum_offset());
            }
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
        width: (PHOTO_SIZE.0 + SIDE_WIDTH + 3.0 * GAP) as u32,
        height: 930,
        graphics,
    })
    .expect("window");
    if let Some(session) = SESSION.lock().unwrap().take() {
        session.stop();
    }
}
