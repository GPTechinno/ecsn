//! SNTP (RFC 4330): the request a client sends and the time it reads from the server's reply,
//! for a board with no clock of its own. The transport, UDP to the server's [`PORT`], is the
//! caller's.

use core::fmt;

/// The server's port.
pub const PORT: u16 = 123;
/// The size of a request, and of a reply without extensions.
pub const PACKET: usize = 48;
/// Seconds from 1900-01-01, the origin of NTP era 0, to the Unix epoch.
const UNIX_OFFSET: i64 = 2_208_988_800;

/// Why a reply gives no time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Shorter than a reply.
    Short,
    /// Not a server's reply (mode 4).
    NotServer,
    /// The server is not synchronised (leap indicator 3), refuses (stratum 0, a kiss-o'-death),
    /// or sends no transmit time.
    Unsynchronised,
    /// Its originate timestamp is not the transmit timestamp of the request: a reply to another
    /// request.
    NotOurs,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::Short => "reply too short",
            Error::NotServer => "not a server's reply",
            Error::Unsynchronised => "server not synchronised",
            Error::NotOurs => "reply to another request",
        })
    }
}

/// A client's request (version 4, mode 3) whose transmit timestamp is `cookie`. The server copies
/// it into its reply's originate timestamp, which ties the reply to this request.
pub fn request(cookie: u64) -> [u8; PACKET] {
    let mut packet = [0; PACKET];
    packet[0] = (4 << 3) | 3;
    packet[40..48].copy_from_slice(&cookie.to_be_bytes());
    packet
}

/// The server's times in a reply, Unix milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reply {
    /// When the request reached the server.
    pub received: i64,
    /// When the reply left it.
    pub transmitted: i64,
}

impl Reply {
    /// The time at which the reply arrived, Unix milliseconds, for a reply that arrived
    /// `round_trip` milliseconds after the request left: the server's transmit time plus half of
    /// the network's share of the round trip, the time the server held the request taken out.
    pub fn arrival(&self, round_trip: u64) -> i64 {
        let held = (self.transmitted - self.received).max(0);
        let network = (round_trip as i64 - held).max(0);
        self.transmitted + network / 2
    }
}

/// Reads the reply to the request made with `cookie`.
pub fn reply(packet: &[u8], cookie: u64) -> Result<Reply, Error> {
    if packet.len() < PACKET {
        return Err(Error::Short);
    }
    let leap = packet[0] >> 6;
    let mode = packet[0] & 7;
    let stratum = packet[1];
    if mode != 4 {
        return Err(Error::NotServer);
    }
    if leap == 3 || stratum == 0 || stratum > 15 || packet[40..48] == [0; 8] {
        return Err(Error::Unsynchronised);
    }
    if packet[24..32] != cookie.to_be_bytes() {
        return Err(Error::NotOurs);
    }
    Ok(Reply {
        received: unix_ms(&packet[32..40]),
        transmitted: unix_ms(&packet[40..48]),
    })
}

/// An NTP timestamp, seconds since 1900 and a 32-bit fraction, as Unix milliseconds. Seconds with
/// their top bit clear are in era 1, which starts on 2036-02-07 (RFC 4330, section 3).
fn unix_ms(timestamp: &[u8]) -> i64 {
    let seconds = u32::from_be_bytes([timestamp[0], timestamp[1], timestamp[2], timestamp[3]]);
    let fraction = u32::from_be_bytes([timestamp[4], timestamp[5], timestamp[6], timestamp[7]]);
    let era = if seconds & 0x8000_0000 == 0 {
        1i64 << 32
    } else {
        0
    };
    let unix = i64::from(seconds) + era - UNIX_OFFSET;
    unix * 1000 + ((u64::from(fraction) * 1000) >> 32) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    const COOKIE: u64 = 0x0123_4567_89ab_cdef;
    /// 2026-11-26 12:00:00 UTC.
    const NOON: i64 = 1_795_694_400;

    fn timestamp(unix: i64, fraction: u32) -> [u8; 8] {
        let seconds = (unix + UNIX_OFFSET) as u32;
        let mut out = [0; 8];
        out[..4].copy_from_slice(&seconds.to_be_bytes());
        out[4..].copy_from_slice(&fraction.to_be_bytes());
        out
    }

    fn server_reply(received: [u8; 8], transmitted: [u8; 8]) -> [u8; PACKET] {
        let mut packet = [0; PACKET];
        packet[0] = (4 << 3) | 4;
        packet[1] = 2;
        packet[24..32].copy_from_slice(&COOKIE.to_be_bytes());
        packet[32..40].copy_from_slice(&received);
        packet[40..48].copy_from_slice(&transmitted);
        packet
    }

    #[test]
    fn a_request_is_version_4_client_with_the_cookie() {
        let packet = request(COOKIE);
        assert_eq!(packet[0], 0x23);
        assert_eq!(packet[1..40], [0; 39]);
        assert_eq!(packet[40..48], COOKIE.to_be_bytes());
    }

    #[test]
    fn a_reply_gives_the_server_times() {
        let packet = server_reply(timestamp(NOON, 0), timestamp(NOON, 0x8000_0000));
        assert_eq!(
            reply(&packet, COOKIE),
            Ok(Reply {
                received: NOON * 1000,
                transmitted: NOON * 1000 + 500,
            })
        );
    }

    #[test]
    fn the_arrival_adds_half_the_network_time() {
        let r = Reply {
            received: 1_000,
            transmitted: 1_010,
        };
        // 50 ms round trip, 10 of them in the server: 20 ms each way
        assert_eq!(r.arrival(50), 1_030);
        // a round trip shorter than the server's hold gives the transmit time
        assert_eq!(r.arrival(5), 1_010);
    }

    #[test]
    fn seconds_with_the_top_bit_clear_are_in_era_1() {
        let mut ts = [0; 8];
        ts[3] = 16;
        // 2036-02-07 06:28:16 UTC is 2^32 - 2 208 988 800 = 2 085 978 496
        assert_eq!(unix_ms(&ts), (2_085_978_496 + 16) * 1000);
    }

    #[test]
    fn replies_that_give_no_time_are_refused() {
        let good = server_reply(timestamp(NOON, 0), timestamp(NOON, 0));
        assert_eq!(reply(&good[..47], COOKIE), Err(Error::Short));
        let mut client = good;
        client[0] = (4 << 3) | 3;
        assert_eq!(reply(&client, COOKIE), Err(Error::NotServer));
        let mut kiss = good;
        kiss[1] = 0;
        assert_eq!(reply(&kiss, COOKIE), Err(Error::Unsynchronised));
        let mut alarm = good;
        alarm[0] |= 3 << 6;
        assert_eq!(reply(&alarm, COOKIE), Err(Error::Unsynchronised));
        let mut silent = good;
        silent[40..48].fill(0);
        assert_eq!(reply(&silent, COOKIE), Err(Error::Unsynchronised));
        assert_eq!(reply(&good, COOKIE ^ 1), Err(Error::NotOurs));
    }
}
