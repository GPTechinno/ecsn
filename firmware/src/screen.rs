//! The display under BLIT: its CPU renderer draws a frame into the LTDC's frame buffer not shown,
//! RGB565, and redraws only the tiles that changed since that buffer last held a frame; the
//! buffers are then swapped at a vertical blanking.

use alloc::string::String;
use alloc::vec;
use core::time::Duration;

use blit::input::Input;
use blit::{FrameInfo, Kernel, Size, Widget};
use blit_cpu::{Pixel, PixelBuffer, PremultipliedRgbaColor, Renderer, RendererConfig};
use blit_gui::text::system::{FontStack, TextConfig, TextSystem};
use blit_gui::text::FontId;
use blit_text::Family;
use embassy_time::Instant;

use crate::display::{Display, HEIGHT, WIDTH};
use crate::text;

/// The font stacks of the text engine: `FontId(0)` sans, `FontId(1)` mono.
pub const SANS: FontId = FontId(0);
pub const MONO: FontId = FontId(1);

/// A pixel of the frame buffers: RGB565, as the LTDC reads them.
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct Rgb565(u16);

impl Pixel for Rgb565 {
    fn from_rgb(red: u8, green: u8, blue: u8) -> Self {
        Self((u16::from(red >> 3) << 11) | (u16::from(green >> 2) << 5) | u16::from(blue >> 3))
    }

    fn to_rgba(self) -> PremultipliedRgbaColor {
        let (r, g, b) = (
            (self.0 >> 11) as u8,
            ((self.0 >> 5) & 0x3F) as u8,
            (self.0 & 0x1F) as u8,
        );
        PremultipliedRgbaColor {
            red: (r << 3) | (r >> 2),
            green: (g << 2) | (g >> 4),
            blue: (b << 3) | (b >> 2),
            alpha: 255,
        }
    }

    fn blend_translucent(&mut self, color: PremultipliedRgbaColor) {
        let below = self.to_rgba();
        let inverse = 255 - u16::from(color.alpha);
        let channel = |back: u8, front: u8| (u16::from(back) * inverse / 255) as u8 + front;
        *self = Self::from_rgb(
            channel(below.red, color.red),
            channel(below.green, color.green),
            channel(below.blue, color.blue),
        );
    }
}

/// The frame buffer the renderer draws into: one of the display's two.
pub struct Frame {
    buffers: [*mut u16; 2],
    back: usize,
}

impl PixelBuffer for Frame {
    type Pixel = Rgb565;

    fn width(&self) -> usize {
        WIDTH
    }

    fn height(&self) -> usize {
        HEIGHT
    }

    fn line_mut(&mut self, line: usize) -> &mut [Rgb565] {
        assert!(line < HEIGHT);
        // SAFETY: each buffer holds WIDTH * HEIGHT pixels for the program's life; the LTDC reads
        // only the other one; `Rgb565` is a transparent u16.
        unsafe {
            core::slice::from_raw_parts_mut(
                self.buffers[self.back].add(line * WIDTH).cast::<Rgb565>(),
                WIDTH,
            )
        }
    }
}

pub type Context = Renderer<Frame>;
pub type Ui<'a> = blit::Ui<'a, Context>;

pub struct Screen {
    display: Display,
    renderer: Context,
    kernel: Kernel<Context>,
    /// Frames drawn: the first two land in buffers no frame was drawn in.
    frames: u32,
}

impl Screen {
    pub fn new(mut display: Display) -> Self {
        let fonts = vec![
            FontStack {
                id: SANS,
                families: vec![Family::named(String::from("sans"))],
            },
            FontStack {
                id: MONO,
                families: vec![Family::named(String::from("mono"))],
            },
        ];
        let config = TextConfig {
            fonts,
            text_cache_capacity: 64 * 1024,
            layout_cache_capacity: 64 * 1024,
        };
        let text = TextSystem::new(config, text::Engine)
            .unwrap_or_else(|_| defmt::panic!("display: the text engine refused its fonts"));
        let frame = Frame {
            buffers: display.buffers(),
            back: 1,
        };
        let renderer = Renderer::new(
            frame,
            text,
            RendererConfig {
                damage_tile_size: 32,
                glyph_cache_capacity: 256 * 1024,
            },
        );
        Self {
            display,
            renderer,
            kernel: Kernel::default(),
            frames: 0,
        }
    }

    /// Draws the frame `widget` builds into the buffer not shown, then shows it.
    pub async fn draw<W: Widget<Context>>(&mut self, widget: W) {
        self.renderer.buffer_mut().back = 1 - self.display.shown();
        self.renderer.reset_frame();
        let time = Duration::from_micros(Instant::now().as_micros());
        let info = FrameInfo::new(Size::new(WIDTH as f32, HEIGHT as f32));
        self.kernel
            .build(&mut self.renderer, info, time, Input::None, widget);
        self.kernel.layout(&mut self.renderer);
        self.kernel.paint(&mut self.renderer);
        // a buffer holds the frame before last, once both have one
        let age = if self.frames >= 2 { 2 } else { 0 };
        self.renderer.render(age);
        self.frames = self.frames.saturating_add(1);
        self.display.swap().await;
    }
}
