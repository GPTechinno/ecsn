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
    let scripts = env::var_os("CARGO_FEATURE_SCRIPTS").is_some();
    // `memory.x` only in OUT_DIR: the linker would take one beside the manifest first
    let memory = fs::read_to_string("memory.x.in").expect("memory.x.in");
    let memory = if scripts {
        // bank 2 too, the Cortex-M4's: bank 1 holds the firmware without the script interpreter
        let bank1 = "LENGTH = 1024K /* bank 1 */";
        assert!(memory.contains(bank1), "memory.x: FLASH {bank1}");
        memory.replace(bank1, "LENGTH = 2048K /* banks 1 and 2 */")
    } else {
        memory
    };
    fs::write(out.join("memory.x"), memory).expect("memory.x");
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x.in");
    println!("cargo:rerun-if-changed=cxx.x");
    println!("cargo:rerun-if-changed=link-cxx.x");
    println!("cargo:rerun-if-changed=build.rs");

    println!("cargo:rustc-link-arg-bins=--nmagic");
    if scripts {
        fs::copy("cxx.x", out.join("cxx.x")).expect("cxx.x");
        fs::copy("link-cxx.x", out.join("link-cxx.x")).expect("link-cxx.x");
        println!("cargo:rustc-link-arg-bins=-Tcxx.x");
        // cortex-m-rt's link.x keeping the exception tables
        println!("cargo:rustc-link-arg-bins=-Tlink-cxx.x");
        // the exception tables' references to type information (R_ARM_TARGET2): PC-relative, as
        // bare-metal libsupc++ reads them, where lld's default is through a GOT
        println!("cargo:rustc-link-arg-bins=--target2=rel");
        cxx_runtime();
    } else {
        println!("cargo:rustc-link-arg-bins=-Tlink.x");
    }
    println!("cargo:rustc-link-arg-bins=-Tdefmt.x");

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    println!("cargo:rustc-env=ECSN_BUILD_TIME={now}");

    glyphs(&out);
}

/// Links Arm's C++ runtime for the Cortex-M7 with the FPU (the multilib `arm-none-eabi-g++` picks
/// for those flags): libstdc++ and libsupc++ (exceptions), newlib, libgcc (the unwinder).
fn cxx_runtime() {
    println!("cargo:rerun-if-env-changed=CXX_thumbv7em_none_eabihf");
    let cxx = env::var("CXX_thumbv7em_none_eabihf").unwrap_or_else(|_| "arm-none-eabi-g++".into());
    let flags = [
        "-mcpu=cortex-m7",
        "-mthumb",
        "-mfpu=fpv5-d16",
        "-mfloat-abi=hard",
    ];
    let dir_of = |query: &str| {
        let output = std::process::Command::new(&cxx)
            .args(flags)
            .arg(query)
            .output()
            .unwrap_or_else(|e| panic!("{cxx}: {e} (the `scripts` feature needs it)"));
        let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        assert!(path.is_absolute(), "{cxx} {query}: not found");
        path.parent().expect("a directory").to_owned()
    };
    println!(
        "cargo:rustc-link-search=native={}",
        dir_of("-print-file-name=libstdc++_nano.a").display()
    );
    println!(
        "cargo:rustc-link-search=native={}",
        dir_of("-print-libgcc-file-name").display()
    );
    for lib in ["stdc++_nano", "supc++_nano", "c_nano", "m", "gcc"] {
        println!("cargo:rustc-link-lib=static={lib}");
    }
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
