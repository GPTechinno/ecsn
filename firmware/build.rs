//! The memory map and linker scripts, `ECSN_BUILD_TIME` (the Unix time this script last ran),
//! and the display's glyphs: DejaVu Sans (regular and bold) and DejaVu Sans Mono from the
//! directory `ECSN_FONTS`, rendered by fontdue at the sizes the display uses, as coverage
//! bitmaps in `glyphs.rs` and `coverage.bin` in `OUT_DIR`.

use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs, path::PathBuf};

use fontdue::{Font, FontSettings};

/// The printable ASCII characters.
const ASCII: &str = concat!(
    " !\"#$%&'()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ",
    "[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~"
);
/// What the large figures and the name need, and `?` for anything else.
const FIGURES: &str = " ,.%0123456789?CENS";

/// The faces: font file, style (0 regular, 1 bold, 2 mono), pixel sizes, characters.
const FACES: &[(&str, u8, &[u16], &str)] = &[
    ("DejaVuSans.ttf", 0, &[11, 13, 15, 18], ASCII),
    ("DejaVuSans-Bold.ttf", 1, &[13, 15, 18, 24], ASCII),
    ("DejaVuSans-Bold.ttf", 1, &[54], FIGURES),
    ("DejaVuSansMono.ttf", 2, &[14], ASCII),
];

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    fs::copy("memory.x", out.join("memory.x")).expect("memory.x");
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-changed=build.rs");

    println!("cargo:rustc-link-arg-bins=--nmagic");
    println!("cargo:rustc-link-arg-bins=-Tlink.x");
    println!("cargo:rustc-link-arg-bins=-Tdefmt.x");

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    println!("cargo:rustc-env=ECSN_BUILD_TIME={now}");

    glyphs(&out);
}

/// Renders the faces into `glyphs.rs` (the faces and glyph metrics) and `coverage.bin` (the
/// glyphs' coverage, row by row, one byte a pixel).
fn glyphs(out: &std::path::Path) {
    println!("cargo:rerun-if-env-changed=ECSN_FONTS");
    let dir = PathBuf::from(env::var("ECSN_FONTS").expect(
        "ECSN_FONTS: the directory of the DejaVu fonts (set by the devenv, from nixpkgs' dejavu_fonts)",
    ));
    let mut faces = String::new();
    let mut glyphs = String::new();
    let mut coverage = Vec::new();
    let mut count = 0;
    for &(file, style, sizes, chars) in FACES {
        let path = dir.join(file);
        let data = fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let font = Font::from_bytes(data, FontSettings::default())
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        for &size in sizes {
            let px = f32::from(size);
            let line = font
                .horizontal_line_metrics(px)
                .unwrap_or_else(|| panic!("{file}: no horizontal metrics"));
            let first = count;
            let mut chars: Vec<char> = chars.chars().collect();
            chars.sort_unstable();
            for ch in chars {
                let (metrics, bitmap) = font.rasterize(ch, px);
                // fontdue's ymin is the bitmap's bottom above the baseline; `top` points down
                let top = -(metrics.ymin + metrics.height as i32);
                writeln!(
                    glyphs,
                    "    Glyph {{ ch: {}, advance: {:?}, left: {}, top: {top}, width: {}, height: {}, offset: {} }},",
                    ch as u32, metrics.advance_width, metrics.xmin, metrics.width, metrics.height,
                    coverage.len()
                )
                .unwrap();
                coverage.extend_from_slice(&bitmap);
                count += 1;
            }
            writeln!(
                faces,
                "    Face {{ style: {style}, size: {size}, ascent: {:?}, line_height: {:?}, glyphs: {first}..{count} }},",
                line.ascent, line.new_line_size
            )
            .unwrap();
        }
    }
    let source = format!(
        "pub static FACES: [Face; {}] = [\n{faces}];\n\npub static GLYPHS: [Glyph; {count}] = [\n{glyphs}];\n",
        faces.lines().count()
    );
    fs::write(out.join("glyphs.rs"), source).expect("glyphs.rs");
    fs::write(out.join("coverage.bin"), coverage).expect("coverage.bin");
}
