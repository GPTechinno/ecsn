//! The firmware logs: defmt frames from RTT up buffer 0, decoded with the
//! table of the firmware's ELF.

use defmt_decoder::{DecodeError, StreamDecoder, Table};

pub struct Logs {
    rtt: Option<u32>,
    decoder: Option<Box<dyn StreamDecoder + Send + Sync + 'static>>,
}

/// The address of the RTT control block in the ELF.
fn rtt_address(elf: &[u8]) -> Option<u32> {
    let obj = goblin::elf::Elf::parse(elf).ok()?;
    obj.syms
        .iter()
        .find(|s| obj.strtab.get_at(s.st_name) == Some("_SEGGER_RTT"))
        .map(|s| s.st_value as u32)
}

impl Logs {
    pub fn new(elf: &[u8]) -> Result<Self, String> {
        let table: &'static Table = match Table::parse(elf) {
            Ok(Some(table)) => Box::leak(Box::new(table)),
            Ok(None) => return Err("the ELF has no defmt table".into()),
            Err(e) => return Err(format!("the defmt table: {e}")),
        };
        Ok(Self {
            rtt: rtt_address(elf),
            decoder: Some(table.new_stream_decoder()),
        })
    }

    /// The address of the RTT control block.
    pub fn rtt(&self) -> Option<u32> {
        self.rtt
    }

    /// Decodes the frames in `bytes` into log lines.
    pub fn decode(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        let Some(decoder) = self.decoder.as_mut() else {
            return lines;
        };
        decoder.received(bytes);
        loop {
            match decoder.decode() {
                Ok(frame) => {
                    let level = frame
                        .level()
                        .map(|l| l.as_str().to_uppercase())
                        .unwrap_or_default();
                    let time = frame
                        .display_timestamp()
                        .map(|t| format!("{t} "))
                        .unwrap_or_default();
                    lines.push(format!("{time}{level:<5} {}", frame.display_message()));
                }
                Err(DecodeError::UnexpectedEof) => break,
                Err(DecodeError::Malformed) => lines.push("(malformed defmt frame)".into()),
            }
        }
        lines
    }
}
