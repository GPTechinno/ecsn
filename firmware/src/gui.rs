//! What the display shows, drawn with BLIT: a start screen with the step the firmware is at,
//! then the node's dashboard: the validated chain, its tip, the node's phase, the peer, the
//! network, the storage.

use alloc::format;
use alloc::string::String;
use core::fmt::Write;

use blit::state::Open;
use blit::Sides;
use blit_gui::color::Color;
use blit_gui::rectangle::Rectangle;
use blit_gui::style::{Border, BorderRadius};
use blit_gui::text::{HorizontalAlign, Text, TextOptions, TextStyle};
use blit_widget::layout::flex;
use blit_widget::layout::{Align, Justify, Sizing};

use crate::screen::{Context, Ui, MONO};

const BACKGROUND: Color = Color::rgb(0x0D, 0x11, 0x17);
const PANEL: Color = Color::rgb(0x16, 0x1B, 0x22);
const EDGE: Color = Color::rgb(0x30, 0x36, 0x3D);
const TEXT: Color = Color::rgb(0xE6, 0xED, 0xF3);
const MUTED: Color = Color::rgb(0x8B, 0x94, 0x9E);
const ORANGE: Color = Color::rgb(0xF7, 0x93, 0x1A);
const GREEN: Color = Color::rgb(0x3F, 0xB9, 0x50);
const YELLOW: Color = Color::rgb(0xD2, 0x99, 0x22);
const RED: Color = Color::rgb(0xF8, 0x51, 0x49);
const TRACK: Color = Color::rgb(0x26, 0x2C, 0x36);

/// A node laid out as a flex column or row, open for children.
type Flex<'a> = blit::Ui<'a, Context, Open<flex::Layout<f32>>>;

/// `part` of `whole` in percent with a decimal, rounded down: `99.9 %`.
fn percent(part: u64, whole: u64) -> String {
    let tenths = part.min(whole) * 1000 / whole.max(1);
    format!("{}.{} %", tenths / 10, tenths % 10)
}

/// Bytes in megabytes with a decimal, rounded down.
fn megabytes(bytes: usize) -> String {
    let tenths = (bytes as u64 * 10) >> 20;
    format!("{}.{}", tenths / 10, tenths % 10)
}

/// `n` with its thousands apart: 1 296 810.
pub fn grouped(n: u64) -> String {
    let digits = format!("{n}");
    let mut out = String::new();
    for (i, d) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(d);
    }
    out
}

/// A Unix time as `2026-11-25 12:31` (UTC).
pub fn date_time(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let seconds = unix.rem_euclid(86_400);
    // days since 1970-01-01 to a civil date (proleptic Gregorian)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        seconds / 3600,
        seconds % 3600 / 60
    )
}

/// `MAC 02:45:43:53:4e:2d`.
pub fn mac_text(mac: [u8; 6]) -> String {
    format!(
        "MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

/// A duration as `3 d 4 h`, `4 h 05 min`, `5 min 07 s` or `7 s`.
pub fn duration(seconds: u64) -> String {
    let (d, h, m, s) = (
        seconds / 86_400,
        seconds % 86_400 / 3600,
        seconds % 3600 / 60,
        seconds % 60,
    );
    if d > 0 {
        format!("{d} d {h} h")
    } else if h > 0 {
        format!("{h} h {m:02} min")
    } else if m > 0 {
        format!("{m} min {s:02} s")
    } else {
        format!("{s} s")
    }
}

/// Text of `size` pixels.
fn text(mut ui: Ui<'_>, s: &str, size: f32, color: Color) {
    ui.insert(Text::new(s).style(TextStyle::new().size(size).color(color)));
}

fn bold(mut ui: Ui<'_>, s: &str, size: f32, color: Color) {
    ui.insert(Text::new(s).style(TextStyle::new().size(size).weight(700).color(color)));
}

fn mono(mut ui: Ui<'_>, s: &str, size: f32, color: Color) {
    ui.insert(Text::new(s).style(TextStyle::new().font(MONO).size(size).color(color)));
}

fn text_right(mut ui: Ui<'_>, s: &str, size: f32, color: Color) {
    ui.insert(
        Text::new(s)
            .style(TextStyle::new().size(size).color(color))
            .options(TextOptions::new().horizontal_align(HorizontalAlign::Right)),
    );
}

/// A section's title.
fn title(ui: Ui<'_>, s: &str) {
    bold(ui, s, 11.0, MUTED);
}

/// A panel: a rounded frame around a column.
fn panel(parent: &mut Flex<'_>, item: flex::Item<f32>, build: impl FnOnce(&mut Flex<'_>)) {
    let mut panel = parent
        .child()
        .item(item)
        .layout(flex::column().gap(6.0).align(Align::Stretch))
        .pad(14.0);
    panel.insert(
        Rectangle::new()
            .background(PANEL)
            .border(Border::solid(1.0, EDGE))
            .radius(BorderRadius::all(8.0)),
    );
    build(&mut panel);
}

/// A bar `width` wide filled to `part` of `whole`.
fn bar(parent: &mut Flex<'_>, width: f32, height: f32, (part, whole): (u64, u64), color: Color) {
    let filled = width * part.min(whole) as f32 / whole.max(1) as f32;
    let mut track = parent
        .child()
        .item(
            flex::item()
                .width(Sizing::fixed(width))
                .height(Sizing::fixed(height)),
        )
        .layout(flex::row());
    track.insert(
        Rectangle::new()
            .background(TRACK)
            .radius(BorderRadius::all(height / 2.0)),
    );
    if filled >= 1.0 {
        track
            .child()
            .item(
                flex::item()
                    .width(Sizing::fixed(filled))
                    .height(Sizing::fixed(height)),
            )
            .build(|mut ui: Ui<'_>| {
                ui.insert(
                    Rectangle::new()
                        .background(color)
                        .radius(BorderRadius::all(height / 2.0)),
                );
            });
    }
}

/// A round light of `color`.
fn dot(parent: &mut Flex<'_>, color: Color) {
    parent
        .child()
        .item(
            flex::item()
                .width(Sizing::fixed(12.0))
                .height(Sizing::fixed(12.0)),
        )
        .build(|mut ui: Ui<'_>| {
            ui.insert(
                Rectangle::new()
                    .background(color)
                    .radius(BorderRadius::all(6.0)),
            );
        });
}

/// The header: the name, the network, the clock and the uptime.
fn header(root: &mut Flex<'_>, network: &str, clock: Option<i64>, uptime: u64) {
    let mut header = root
        .child()
        .item(flex::item().height(Sizing::fixed(46.0)))
        .layout(flex::row().gap(14.0).align(Align::Center))
        .pad(Sides::new().left(16.0).right(16.0));
    header.insert(Rectangle::new().background(PANEL));
    header
        .child()
        .build(|ui: Ui<'_>| bold(ui, "ECSN", 24.0, ORANGE));
    header
        .child()
        .build(|ui: Ui<'_>| text(ui, "Embedded Compact State Node", 13.0, MUTED));
    header
        .child()
        .item(flex::item().grow())
        .build(|_: Ui<'_>| {});
    let badge: String = network.chars().map(|c| c.to_ascii_uppercase()).collect();
    {
        let mut tag = header
            .child()
            .layout(flex::row())
            .pad(Sides::new().top(3.0).bottom(3.0).left(10.0).right(10.0));
        tag.insert(
            Rectangle::new()
                .border(Border::solid(1.0, ORANGE))
                .radius(BorderRadius::all(6.0)),
        );
        tag.child()
            .build(|ui: Ui<'_>| bold(ui, &badge, 13.0, ORANGE));
    }
    header
        .child()
        .item(flex::item().grow())
        .build(|_: Ui<'_>| {});
    let mut right = header
        .child()
        .layout(flex::column().gap(2.0).align(Align::End));
    // SNTP's clock, or the peer's until SNTP answers: none before either
    if let Some(unix) = clock {
        let now = format!("{} UTC", date_time(unix));
        right.child().build(|ui: Ui<'_>| bold(ui, &now, 15.0, TEXT));
    }
    let up = format!("up {}", duration(uptime));
    right.child().build(|ui: Ui<'_>| text(ui, &up, 11.0, MUTED));
}

/// The start screen: the step the firmware is at, and details under it.
pub fn start(ui: Ui<'_>, network: &str, step: &str, details: &[&str], uptime: u64) {
    let mut root = ui.layout(flex::column().align(Align::Stretch));
    root.insert(Rectangle::new().background(BACKGROUND));
    header(&mut root, network, None, uptime);
    let mut body = root.child().item(flex::item().grow()).layout(
        flex::column()
            .gap(10.0)
            .align(Align::Center)
            .justify(Justify::Center),
    );
    body.child()
        .build(|ui: Ui<'_>| bold(ui, "ECSN", 54.0, ORANGE));
    body.child()
        .build(|ui: Ui<'_>| text(ui, "a Utreexo node on a microcontroller", 15.0, MUTED));
    body.child()
        .item(flex::item().height(Sizing::fixed(28.0)))
        .build(|_: Ui<'_>| {});
    body.child().build(|ui: Ui<'_>| bold(ui, step, 18.0, TEXT));
    for line in details {
        body.child().build(|ui: Ui<'_>| text(ui, line, 15.0, MUTED));
    }
}

/// Everything the dashboard shows.
pub struct Dashboard<'a> {
    pub network: &'a str,
    pub clock: Option<i64>,
    pub uptime: u64,
    pub address: Option<String>,
    pub mac: [u8; 6],
    pub status: &'a ecsn::node::Status,
    pub best_header: u32,
    pub validated: u32,
    /// The validated tip's hash and time.
    pub tip: Option<(String, u32)>,
    /// The height of the accumulator taken as valid, if any.
    pub assumed: Option<u32>,
    pub leaves: u64,
    pub roots: usize,
    /// Headers the flash has room for; `None` with the chain in RAM.
    pub capacity: Option<u32>,
    /// Heap used and its size, bytes.
    pub heap: (usize, usize),
}

/// The node's phase in words, and its color.
fn phase(d: &Dashboard) -> (&'static str, Color) {
    use ecsn::node::Phase;
    match d.status.phase {
        Phase::Idle if d.status.bridge().is_some() => ("Connecting to a bridge", YELLOW),
        Phase::Idle => ("No bridge, trying again", RED),
        Phase::Handshake => ("Handshake with the bridge", YELLOW),
        Phase::Headers => ("Syncing headers", YELLOW),
        Phase::Assume => ("Assuming the accumulator", YELLOW),
        Phase::Blocks => ("Validating blocks", YELLOW),
        Phase::Following => ("Following the chain", GREEN),
    }
}

/// A user agent's last name and version, without its comment: `utreexod:0.5.1` of
/// `/btcwire:0.5.0/utreexod:0.5.1/`, 24 characters at most.
fn agent(user_agent: &str) -> &str {
    let last = user_agent
        .trim_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("");
    let last = last.split('(').next().unwrap_or(last);
    let end = last.char_indices().nth(24).map_or(last.len(), |(i, _)| i);
    if last.is_empty() {
        "?"
    } else {
        &last[..end]
    }
}

/// The left column's width, and its panels' inner width.
const LEFT: f32 = 452.0;
const LEFT_INNER: f32 = LEFT - 28.0;
/// The right column's panel's inner width.
const RIGHT_INNER: f32 = 800.0 - 3.0 * 12.0 - LEFT - 28.0;

pub fn dashboard(ui: Ui<'_>, d: &Dashboard) {
    let mut root = ui.layout(flex::column().gap(12.0).align(Align::Stretch));
    root.insert(Rectangle::new().background(BACKGROUND));
    header(&mut root, d.network, d.clock, d.uptime);
    {
        let mut body = root
            .child()
            .item(flex::item().grow())
            .layout(flex::row().gap(12.0))
            .pad(Sides::new().left(12.0).right(12.0));
        {
            let mut left = body
                .child()
                .item(flex::item().width(Sizing::fixed(LEFT)))
                .layout(flex::column().gap(12.0).align(Align::Stretch));
            panel(&mut left, flex::item(), |p| validated(p, d));
            panel(&mut left, flex::item().grow(), |p| tip(p, d));
        }
        panel(&mut body, flex::item().grow(), |p| node(p, d));
    }
    let mut footer = root
        .child()
        .layout(flex::column().gap(4.0))
        .pad(Sides::new().left(16.0).right(16.0).bottom(10.0));
    let acc = format!(
        "Accumulator: {} leaves, {} roots",
        grouped(d.leaves),
        d.roots
    );
    footer
        .child()
        .build(|ui: Ui<'_>| text(ui, &acc, 11.0, MUTED));
    footer.child().build(|ui: Ui<'_>| {
        text(
            ui,
            "Blocks checked for proof of work, Utreexo proofs and amounts; scripts are not checked",
            11.0,
            MUTED,
        )
    });
}

/// The validated chain: its height out of the best header's, and the progress.
fn validated(p: &mut Flex<'_>, d: &Dashboard) {
    {
        let mut top = p.child().layout(flex::row().justify(Justify::SpaceBetween));
        top.child().build(|ui: Ui<'_>| title(ui, "VALIDATED"));
        if let Some(assumed) = d.assumed {
            let s = format!("assumed at {}", grouped(u64::from(assumed)));
            top.child()
                .build(|ui: Ui<'_>| text_right(ui, &s, 11.0, MUTED));
        }
    }
    let height = grouped(u64::from(d.validated));
    p.child().build(|ui: Ui<'_>| bold(ui, &height, 54.0, TEXT));
    let of = format!("of {} headers", grouped(u64::from(d.best_header)));
    p.child().build(|ui: Ui<'_>| text(ui, &of, 15.0, MUTED));
    let done = (u64::from(d.validated), u64::from(d.best_header));
    let synced = d.validated >= d.best_header;
    {
        let mut row = p.child().layout(flex::row().gap(12.0).align(Align::Center));
        bar(
            &mut row,
            LEFT_INNER - 76.0,
            10.0,
            done,
            if synced { GREEN } else { ORANGE },
        );
        let pct = percent(done.0, done.1);
        row.child()
            .item(flex::item().width(Sizing::grow()))
            .build(|ui: Ui<'_>| {
                let mut ui = ui;
                ui.insert(
                    Text::new(pct.as_str())
                        .style(TextStyle::new().size(15.0).weight(700).color(TEXT))
                        .options(TextOptions::new().horizontal_align(HorizontalAlign::Right)),
                );
            });
    }
    let behind = d.best_header.saturating_sub(d.validated);
    let note = if behind == 0 {
        String::from("at the best header")
    } else {
        format!("{} blocks to go", grouped(u64::from(behind)))
    };
    p.child().build(|ui: Ui<'_>| text(ui, &note, 13.0, MUTED));
}

/// The validated tip: its hash, time and age, and the last block's contents.
fn tip(p: &mut Flex<'_>, d: &Dashboard) {
    p.child().build(|ui: Ui<'_>| title(ui, "TIP"));
    match &d.tip {
        Some((hash, time)) => {
            let (first, second) = hash.split_at(32);
            p.child().build(|ui: Ui<'_>| mono(ui, first, 14.0, TEXT));
            p.child().build(|ui: Ui<'_>| mono(ui, second, 14.0, TEXT));
            let mut when = format!("{} UTC", date_time(i64::from(*time)));
            if let Some(now) = d.clock {
                let age = now - i64::from(*time);
                if age >= 0 {
                    let _ = write!(when, ", {} ago", duration(age as u64));
                }
            }
            p.child().build(|ui: Ui<'_>| text(ui, &when, 13.0, MUTED));
        }
        None => p
            .child()
            .build(|ui: Ui<'_>| text(ui, "none validated yet", 13.0, MUTED)),
    }
    if let Some(last) = &d.status.last_block {
        let line = format!(
            "{} tx, {} inputs proven, {} kB",
            grouped(last.transactions as u64),
            grouped(last.inputs as u64),
            grouped(last.size as u64 / 1000)
        );
        p.child().build(|ui: Ui<'_>| text(ui, &line, 13.0, MUTED));
    }
}

/// The node: its phase, the peer, the network, the storage.
fn node(p: &mut Flex<'_>, d: &Dashboard) {
    p.child().build(|ui: Ui<'_>| title(ui, "NODE"));
    let (words, color) = phase(d);
    {
        let mut row = p.child().layout(flex::row().gap(8.0).align(Align::Center));
        dot(&mut row, color);
        row.child().build(|ui: Ui<'_>| bold(ui, words, 15.0, TEXT));
    }
    let session = format!(
        "{} validated since the start",
        grouped(u64::from(d.status.validated))
    );
    p.child()
        .build(|ui: Ui<'_>| text(ui, &session, 11.0, MUTED));
    gap(p);

    {
        let mut top = p.child().layout(flex::row().justify(Justify::SpaceBetween));
        top.child().build(|ui: Ui<'_>| title(ui, "PEERS"));
        let known = format!("{} known", grouped(d.status.known as u64));
        top.child()
            .build(|ui: Ui<'_>| text_right(ui, &known, 11.0, MUTED));
    }
    if d.status.peers.is_empty() {
        p.child()
            .build(|ui: Ui<'_>| text(ui, "none connected", 11.0, MUTED));
    }
    for peer in &d.status.peers {
        use ecsn::node::Role;
        let ip = peer
            .addr
            .map_or_else(|| String::from("?"), |a| format!("{}", a.ip()));
        let v2 = if peer.v2 { " v2" } else { "" };
        let what = match (&peer.info, peer.role) {
            (Some(info), Role::Bridge) => format!("bridge, {}{v2}", agent(&info.user_agent)),
            (Some(info), Role::Peer) => format!("{}{v2}", agent(&info.user_agent)),
            (None, Role::Bridge) => String::from("bridge, connecting"),
            (None, Role::Peer) => String::from("connecting"),
        };
        let color = if peer.info.is_some() { GREEN } else { YELLOW };
        let mut row = p.child().layout(flex::row().gap(8.0).align(Align::Center));
        dot(&mut row, color);
        row.child().build(|ui: Ui<'_>| bold(ui, &ip, 13.0, TEXT));
        row.child()
            .item(flex::item().grow())
            .build(|ui: Ui<'_>| text_right(ui, &what, 11.0, MUTED));
    }
    gap(p);

    p.child().build(|ui: Ui<'_>| title(ui, "ETHERNET"));
    let address = d.address.as_deref().unwrap_or("no address yet");
    p.child().build(|ui: Ui<'_>| bold(ui, address, 15.0, TEXT));
    let mac = mac_text(d.mac);
    p.child().build(|ui: Ui<'_>| text(ui, &mac, 11.0, MUTED));
    gap(p);

    p.child().build(|ui: Ui<'_>| title(ui, "STORAGE"));
    match d.capacity {
        Some(capacity) => {
            let stored = d.best_header + 1;
            let flash = format!(
                "Flash {} / {} headers",
                grouped(u64::from(stored)),
                grouped(u64::from(capacity))
            );
            p.child().build(|ui: Ui<'_>| text(ui, &flash, 13.0, TEXT));
            bar(
                p,
                RIGHT_INNER,
                6.0,
                (u64::from(stored), u64::from(capacity)),
                ORANGE,
            );
        }
        None => p
            .child()
            .build(|ui: Ui<'_>| text(ui, "Chain in RAM", 13.0, TEXT)),
    }
    let (used, size) = d.heap;
    let heap = format!("Heap {} / {} MB", megabytes(used), size >> 20);
    p.child().build(|ui: Ui<'_>| text(ui, &heap, 13.0, TEXT));
    bar(p, RIGHT_INNER, 6.0, (used as u64, size as u64), ORANGE);
}

/// Space between the sections of a panel.
fn gap(p: &mut Flex<'_>) {
    p.child()
        .item(flex::item().height(Sizing::fixed(4.0)))
        .build(|_: Ui<'_>| {});
}
