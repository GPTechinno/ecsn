//! `tracing` events (those of `ecsn` and `floresta-chain`) printed through defmt. Spans are
//! ignored.

use alloc::string::String;
use core::fmt::{self, Write};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};

/// The most detailed level printed: `ECSN_LOG` at build time (`error`, `warn`, `info`, `debug`
/// or `trace`), `info` by default.
const MAX_LEVEL: Level = match option_env!("ECSN_LOG") {
    Some(level) => match level.as_bytes() {
        b"error" => Level::ERROR,
        b"warn" => Level::WARN,
        b"debug" => Level::DEBUG,
        b"trace" => Level::TRACE,
        _ => Level::INFO,
    },
    None => Level::INFO,
};

struct Defmt;

impl Subscriber for Defmt {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        *metadata.level() <= MAX_LEVEL
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut line = String::new();
        event.record(&mut Line(&mut line));
        let target = event.metadata().target();
        match *event.metadata().level() {
            Level::ERROR => defmt::error!("{=str}: {=str}", target, line),
            Level::WARN => defmt::warn!("{=str}: {=str}", target, line),
            Level::INFO => defmt::info!("{=str}: {=str}", target, line),
            Level::DEBUG => defmt::debug!("{=str}: {=str}", target, line),
            Level::TRACE => defmt::trace!("{=str}: {=str}", target, line),
        }
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

/// An event's message, then its other fields as `name=value`.
struct Line<'a>(&'a mut String);

impl Visit for Line<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        let _ = if field.name() == "message" {
            write!(self.0, "{value:?}")
        } else {
            write!(self.0, "{}={value:?}", field.name())
        };
    }
}

/// Sends `tracing` events to defmt. Needs the heap.
pub fn init() {
    let _ = tracing::dispatcher::set_global_default(tracing::Dispatch::new(Defmt));
}
