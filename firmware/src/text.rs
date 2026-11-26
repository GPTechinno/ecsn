//! BLIT's text engine on the board: glyphs rendered at build time (see `build.rs`), DejaVu Sans
//! regular and bold and DejaVu Sans Mono at a few pixel sizes. A text takes the face of its
//! style nearest in size; it is laid out by the glyphs' advances, without kerning, one line per
//! `\n`, clipped (no wrapping or ellipsis). Characters outside the face's set show as `?`.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ops::Range;

use blit::{LogicalPoint, LogicalRect, LogicalSize, PhysicalPoint};
use blit_text::{
    Caret, Family, FontError, FontStackId, Glyph as LaidGlyph, GlyphImage, GlyphMetrics,
    GlyphPixels, GlyphRequest, LayoutLine, LayoutRequest, LayoutRun, PreparedGlyph, Text,
    TextLayout, TextLayoutEngine,
};

/// A face at one pixel size: its glyphs are `GLYPHS[glyphs]`, sorted by character.
pub struct Face {
    /// 0 regular, 1 bold, 2 mono.
    pub style: u8,
    pub size: u16,
    /// Above the baseline, pixels.
    pub ascent: f32,
    pub line_height: f32,
    pub glyphs: Range<usize>,
}

/// A glyph's metrics: its coverage is `COVERAGE[offset..offset + width * height]`.
pub struct Glyph {
    pub ch: u32,
    pub advance: f32,
    /// The bitmap's top left from the pen on the baseline, pixels, `top` pointing down.
    pub left: i32,
    pub top: i32,
    pub width: usize,
    pub height: usize,
    pub offset: usize,
}

include!(concat!(env!("OUT_DIR"), "/glyphs.rs"));
static COVERAGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/coverage.bin"));

/// The font stacks: named `"sans"` (regular, bold from weight 600) or `"mono"`.
const SANS: u64 = 0;
const MONO: u64 = 1;

/// Glyphs drawn in place of characters a face lacks.
const MISSING: u32 = '?' as u32;

/// A shaped text: the byte range and face of each span, in order.
pub struct Shape {
    spans: Vec<Range<usize>>,
    faces: Vec<usize>,
}

/// A glyph to rasterize: its face and character.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    face: u16,
    ch: u32,
}

pub struct Engine;

impl Face {
    fn glyph(&self, ch: u32) -> Option<&'static Glyph> {
        let glyphs = &GLYPHS[self.glyphs.clone()];
        glyphs
            .binary_search_by_key(&ch, |g| g.ch)
            .ok()
            .map(|i| &glyphs[i])
    }

    /// The glyph of `ch`, or of `?`, or none for a face without either.
    fn glyph_or_missing(&self, ch: u32) -> Option<&'static Glyph> {
        self.glyph(ch).or_else(|| self.glyph(MISSING))
    }
}

/// The face of `style` nearest `size`.
fn face(style: u8, size: f32) -> usize {
    let distance = |face: &Face| {
        let d = f32::from(face.size) - size;
        if d < 0.0 {
            -d
        } else {
            d
        }
    };
    let mut best = 0;
    for (i, face) in FACES.iter().enumerate() {
        if face.style == style
            && (FACES[best].style != style || distance(face) < distance(&FACES[best]))
        {
            best = i;
        }
    }
    best
}

/// `value` to the nearest integer, halves away from zero.
fn round(value: f32) -> i32 {
    let truncated = value as i32;
    let rest = value - truncated as f32;
    if rest >= 0.5 {
        truncated + 1
    } else if rest <= -0.5 {
        truncated - 1
    } else {
        truncated
    }
}

impl TextLayoutEngine for Engine {
    type Shape = Shape;
    type GlyphKey = GlyphKey;

    fn register_stack(&mut self, families: &[Family]) -> Result<FontStackId, FontError> {
        match families.first() {
            Some(Family::Named(name)) if name == "sans" => Ok(FontStackId(SANS)),
            Some(Family::Named(name)) if name == "mono" => Ok(FontStackId(MONO)),
            _ => Err(FontError::NotFound),
        }
    }

    fn shape(&mut self, text: Text<'_>) -> (Shape, usize) {
        let spans: Vec<Range<usize>> = text.segments().map(|(_, range, _)| range).collect();
        let faces: Vec<usize> = text
            .segments()
            .map(|(_, _, style)| {
                let face_style = match (style.font.0, style.weight) {
                    (MONO, _) => 2,
                    (_, weight) if weight >= 600 => 1,
                    _ => 0,
                };
                face(face_style, style.size)
            })
            .collect();
        let weight = faces.len() * core::mem::size_of::<(Range<usize>, usize)>();
        (Shape { spans, faces }, weight)
    }

    fn layout(&mut self, shape: &mut Shape, text: &str, request: LayoutRequest) -> TextLayout {
        // one line height and baseline for the text: those of its largest face
        let (ascent, line_height) = shape.faces.iter().fold((0.0f32, 0.0f32), |(a, h), &f| {
            let face = &FACES[f];
            (a.max(face.ascent), h.max(face.line_height))
        });
        let max_lines = request.max_lines.map_or(usize::MAX, usize::from);
        let mut glyphs = Vec::new();
        let mut runs = Vec::new();
        let mut lines: Vec<LayoutLine> = Vec::new();
        let mut x = 0.0f32;
        let mut line_start = 0usize;
        let mut width = 0.0f32;
        let finish_line = |lines: &mut Vec<LayoutLine>, x: f32, end: usize, start: &mut usize| {
            let y = lines.len() as f32 * line_height;
            lines.push(LayoutLine {
                bounds: LogicalRect::new(0.0, y, x, line_height),
                text: *start as u32..end as u32,
            });
            *start = end + 1;
        };
        'text: for (span, range) in shape.spans.iter().cloned().enumerate() {
            let face = &FACES[shape.faces[span]];
            let mut run_start = glyphs.len() as u32;
            for (offset, ch) in text[range.clone()].char_indices() {
                if ch == '\n' {
                    if glyphs.len() as u32 > run_start {
                        runs.push(LayoutRun {
                            font: span,
                            glyphs: run_start..glyphs.len() as u32,
                            span,
                            line: lines.len() as u32,
                        });
                    }
                    width = width.max(x);
                    finish_line(&mut lines, x, range.start + offset, &mut line_start);
                    x = 0.0;
                    run_start = glyphs.len() as u32;
                    if lines.len() >= max_lines {
                        break 'text;
                    }
                    continue;
                }
                let Some(glyph) = face.glyph_or_missing(ch as u32) else {
                    continue;
                };
                let baseline = lines.len() as f32 * line_height + ascent;
                glyphs.push(LaidGlyph {
                    id: glyph.ch as u16,
                    position: LogicalPoint::new(x, baseline),
                });
                x += glyph.advance;
            }
            if glyphs.len() as u32 > run_start {
                runs.push(LayoutRun {
                    font: span,
                    glyphs: run_start..glyphs.len() as u32,
                    span,
                    line: lines.len() as u32,
                });
            }
        }
        if lines.len() < max_lines {
            width = width.max(x);
            finish_line(&mut lines, x, text.len(), &mut line_start);
        }
        TextLayout {
            size: LogicalSize::new(width, lines.len() as f32 * line_height),
            glyphs: glyphs.into_boxed_slice(),
            runs: runs.into_boxed_slice(),
            lines: lines.into_boxed_slice(),
        }
    }

    fn prepare_glyph(&mut self, shape: &Shape, request: GlyphRequest) -> PreparedGlyph<GlyphKey> {
        PreparedGlyph {
            key: GlyphKey {
                face: shape.faces[request.font] as u16,
                ch: u32::from(request.glyph),
            },
            origin: PhysicalPoint::new(round(request.origin.x), round(request.origin.y)),
        }
    }

    fn rasterize_glyph(&mut self, key: &GlyphKey) -> Result<GlyphImage, FontError> {
        let face = FACES
            .get(usize::from(key.face))
            .ok_or(FontError::InvalidData)?;
        let glyph = face.glyph(key.ch).ok_or(FontError::NotFound)?;
        let bytes = &COVERAGE[glyph.offset..glyph.offset + glyph.width * glyph.height];
        Ok(GlyphImage {
            metrics: GlyphMetrics {
                width: glyph.width as u16,
                height: glyph.height as u16,
                left: glyph.left as i16,
                top: glyph.top as i16,
            },
            pixels: GlyphPixels::Alpha8(Box::from(bytes)),
        })
    }

    fn carets(&mut self, _: &mut Shape, _: &str, _: LayoutRequest, _: usize) -> Box<[Caret]> {
        Box::default()
    }
}
