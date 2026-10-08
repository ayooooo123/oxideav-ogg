//! Whole audio frames rebuilt from byte chunks that ignore frame
//! boundaries, as OGM muxers cut AC-3 and MPEG audio (FFmpeg reads such
//! streams through its audio parsers, `AVSTREAM_PARSE_FULL`).
//!
//! Frame sizes follow the specifications: AC-3 from ATSC A/52 Table 5.18
//! (`fscod`, `frmsizecod`; half and quarter rates for `bsid` 9 and 10),
//! E-AC-3 from A/52 Annex E (`frmsiz`, `numblkscod`, `fscod2`), MPEG audio
//! from ISO/IEC 11172-3 §2.4.3.1 and 13818-3 (bitrate, sampling frequency,
//! padding). Bytes before a valid header are skipped. A chunk's pts goes
//! to the first frame starting inside the chunk, as FFmpeg's parsers
//! assign timestamps; other frames have none. A frame the stream ends
//! inside never plays. Each E-AC-3 syncframe is its own frame; dependent
//! substreams are not joined to their independent frame.

use std::collections::VecDeque;

/// The longest frame any accepted header declares (E-AC-3: 2048 words).
const MAX_FRAME: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// AC-3 and E-AC-3 (sync word 0x0B77).
    Ac3,
    /// MPEG-1/2/2.5 audio layers I–III.
    Mpeg,
}

/// One frame: its bytes, the pts it starts at (if a chunk gave one), and
/// its length in samples at `sample_rate`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Frame {
    pub(crate) data: Vec<u8>,
    pub(crate) pts: Option<i64>,
    pub(crate) samples: u32,
    pub(crate) sample_rate: u32,
}

/// The framer for one stream.
pub(crate) struct Framer {
    kind: Kind,
    buf: Vec<u8>,
    /// Stream offset of `buf[0]`.
    base: u64,
    /// Chunks with a pts, `[start, end)` in stream offsets, whose pts no
    /// frame has taken yet.
    stamps: VecDeque<(u64, u64, i64)>,
}

impl Framer {
    /// The framer for a WAVE format tag: AC-3 (0x2000), MPEG audio layers
    /// I/II (0x0050) and MP3 (0x0055); other codecs need none.
    pub(crate) fn for_wave_tag(tag: u16) -> Option<Self> {
        let kind = match tag {
            0x2000 => Kind::Ac3,
            0x0050 | 0x0055 => Kind::Mpeg,
            _ => return None,
        };
        Some(Framer { kind, buf: Vec::new(), base: 0, stamps: VecDeque::new() })
    }

    /// Forgets buffered bytes and timestamps (a seek).
    pub(crate) fn reset(&mut self) {
        self.buf.clear();
        self.stamps.clear();
    }

    /// Takes the next chunk and appends every frame it completes to `out`.
    pub(crate) fn push(&mut self, chunk: &[u8], pts: Option<i64>, out: &mut Vec<Frame>) {
        let start = self.base + self.buf.len() as u64;
        if let Some(pts) = pts {
            self.stamps.push_back((start, start + chunk.len() as u64, pts));
        }
        self.buf.extend_from_slice(chunk);
        let mut at = 0;
        loop {
            // The first valid header at or after `at`.
            let Some((skip, header)) = (at..self.buf.len()).find_map(|i| self.header(&self.buf[i..]).map(|h| (i, h)))
            else {
                // Keep the bytes that may still start a header (an AC-3
                // header check reads 6).
                at = self.buf.len().saturating_sub(5).max(at);
                break;
            };
            at = skip;
            let (len, samples, sample_rate) = header;
            if self.buf.len() - at < len {
                break;
            }
            let frame_start = self.base + at as u64;
            while self.stamps.front().is_some_and(|&(_, end, _)| end <= frame_start) {
                self.stamps.pop_front();
            }
            let pts = match self.stamps.front() {
                Some(&(begin, _, pts)) if begin <= frame_start => {
                    self.stamps.pop_front();
                    Some(pts)
                }
                _ => None,
            };
            out.push(Frame { data: self.buf[at..at + len].to_vec(), pts, samples, sample_rate });
            at += len;
        }
        self.buf.drain(..at);
        self.base += at as u64;
        while self.stamps.front().is_some_and(|&(_, end, _)| end <= self.base) {
            self.stamps.pop_front();
        }
    }

    /// `(frame bytes, samples, sample rate)` of a valid header at the start
    /// of `b`.
    fn header(&self, b: &[u8]) -> Option<(usize, u32, u32)> {
        let parsed = match self.kind {
            Kind::Ac3 => ac3(b),
            Kind::Mpeg => mpeg(b),
        };
        parsed.filter(|&(len, _, _)| len <= MAX_FRAME)
    }
}

/// An AC-3 or E-AC-3 syncframe header.
fn ac3(b: &[u8]) -> Option<(usize, u32, u32)> {
    if b.len() < 6 || b[0] != 0x0B || b[1] != 0x77 {
        return None;
    }
    let bsid = b[5] >> 3;
    let fscod = b[4] >> 6;
    match bsid {
        0..=10 => {
            const RATES: [u32; 3] = [48_000, 44_100, 32_000];
            // A/52 Table 5.18: the nominal bit rates of frmsizecod / 2.
            const KBPS: [u32; 19] = [32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640];
            let frmsizecod = usize::from(b[4] & 0x3F);
            let kbps = *KBPS.get(frmsizecod / 2)?;
            let words = match fscod {
                0 => kbps * 2,
                1 => kbps * 1_536_000 / 705_600 + (frmsizecod as u32 & 1),
                2 => kbps * 3,
                _ => return None,
            };
            // bsid 9 and 10: half and quarter sampling rates.
            let shift = bsid.saturating_sub(8);
            Some((words as usize * 2, 1536, RATES[usize::from(fscod)] >> shift))
        }
        11..=16 => {
            let words = ((usize::from(b[2]) & 7) << 8 | usize::from(b[3])) + 1;
            if words * 2 < 7 {
                return None;
            }
            let code = (b[4] >> 4) & 3;
            let (rate, blocks) = if fscod == 3 {
                ([24_000, 22_050, 16_000].get(usize::from(code)).copied()?, 6)
            } else {
                ([48_000, 44_100, 32_000][usize::from(fscod)], [1, 2, 3, 6][usize::from(code)])
            };
            Some((words * 2, blocks * 256, rate))
        }
        _ => None,
    }
}

/// An MPEG audio frame header; free-format frames are not recognised.
fn mpeg(b: &[u8]) -> Option<(usize, u32, u32)> {
    if b.len() < 4 || b[0] != 0xFF || b[1] & 0xE0 != 0xE0 {
        return None;
    }
    let version = (b[1] >> 3) & 3; // 0: MPEG-2.5, 2: MPEG-2, 3: MPEG-1
    let layer = 4 - ((b[1] >> 1) & 3); // field 3: layer I, 2: II, 1: III
    let bitrate_index = usize::from(b[2] >> 4);
    let rate_index = usize::from((b[2] >> 2) & 3);
    if version == 1 || layer == 4 || bitrate_index == 0 || bitrate_index == 15 || rate_index == 3 {
        return None;
    }
    let mpeg1 = version == 3;
    const KBPS: [[u32; 15]; 5] = [
        [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448],
        [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384],
        [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320],
        [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
    ];
    let table = match (mpeg1, layer) {
        (true, l) => usize::from(l) - 1,
        (false, 1) => 3,
        (false, _) => 4,
    };
    let bitrate = KBPS[table][bitrate_index] * 1000;
    let rate = [44_100, 48_000, 32_000][rate_index] >> match version {
        3 => 0,
        2 => 1,
        _ => 2,
    };
    let padding = u32::from((b[2] >> 1) & 1);
    let (len, samples) = match layer {
        1 => ((12 * bitrate / rate + padding) * 4, 384),
        2 => (144 * bitrate / rate + padding, 1152),
        _ if mpeg1 => (144 * bitrate / rate + padding, 1152),
        _ => (72 * bitrate / rate + padding, 576),
    };
    Some((len as usize, samples, rate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_sizes_follow_the_specifications() {
        // AC-3 192 kbps at 48 kHz (768 bytes), 44.1 kHz odd code, 32 kHz.
        assert_eq!(ac3(&[0x0B, 0x77, 0, 0, 20, 8 << 3]), Some((768, 1536, 48_000)));
        assert_eq!(ac3(&[0x0B, 0x77, 0, 0, 0x40 | 37, 8 << 3]), Some((1394 * 2, 1536, 44_100)));
        assert_eq!(ac3(&[0x0B, 0x77, 0, 0, 0x80 | 36, 8 << 3]), Some((3840, 1536, 32_000)));
        // Reserved fscod, frmsizecod past the table, bsid past E-AC-3.
        assert_eq!(ac3(&[0x0B, 0x77, 0, 0, 0xC0, 8 << 3]), None);
        assert_eq!(ac3(&[0x0B, 0x77, 0, 0, 38, 8 << 3]), None);
        assert_eq!(ac3(&[0x0B, 0x77, 0, 0, 0, 17 << 3]), None);
        // E-AC-3: frmsiz 383 (768 bytes), 6 blocks at 48 kHz; fscod2 24 kHz.
        assert_eq!(ac3(&[0x0B, 0x77, 0x01, 0x7F, 0x30, 16 << 3]), Some((768, 1536, 48_000)));
        assert_eq!(ac3(&[0x0B, 0x77, 0x01, 0x7F, 0xC0, 16 << 3]), Some((768, 1536, 24_000)));
        // MP3 128 kbps 44.1 kHz with padding; MPEG-2 layer III; layer I.
        assert_eq!(mpeg(&[0xFF, 0xFB, 0x92, 0]), Some((418, 1152, 44_100)));
        assert_eq!(mpeg(&[0xFF, 0xF3, 0x90, 0]), Some((72 * 80_000 / 22_050, 576, 22_050)));
        assert_eq!(mpeg(&[0xFF, 0xFF, 0x94, 0]), Some(((12 * 288_000 / 48_000) * 4, 384, 48_000)));
        // Free format, bad bitrate, reserved rate and version.
        assert_eq!(mpeg(&[0xFF, 0xFB, 0x00, 0]), None);
        assert_eq!(mpeg(&[0xFF, 0xFB, 0xF0, 0]), None);
        assert_eq!(mpeg(&[0xFF, 0xFB, 0x9C, 0]), None);
        assert_eq!(mpeg(&[0xFF, 0xEB, 0x90, 0]), None);
    }

    #[test]
    fn junk_without_a_header_stays_bounded() {
        let mut f = Framer::for_wave_tag(0x2000).unwrap();
        let mut out = Vec::new();
        for _ in 0..100 {
            f.push(&[0x55; 10_000], Some(7), &mut out);
        }
        assert!(out.is_empty());
        // The last chunk's stamp stays while its kept bytes may start a frame.
        assert!(f.buf.len() <= 5 && f.stamps.len() <= 1, "{} bytes, {} stamps held", f.buf.len(), f.stamps.len());
    }
}
