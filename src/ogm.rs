// SPDX-License-Identifier: MIT
// Port of FFmpeg 2da55bf libavformat/oggparseogm.c (ogm_header,
// ogm_dshow_header, ogm_packet and its codec table).
// Copyright (C) 2005  Michael Ahlberg, Måns Rullgård
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the
// "Software"), to deal in the Software without restriction, including
// without limitation the rights to use, copy, modify, merge, publish,
// distribute, sublicense, and/or sell copies of the Software, and to permit
// persons to whom the Software is furnished to do so, subject to the
// following conditions:
//
// The above copyright notice and this permission notice shall be included
// in all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
// DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR
// OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE
// USE OR OTHER DEALINGS IN THE SOFTWARE.

//! OGM ("Ogg Media", OggDS): DirectShow-style stream headers in Ogg, read
//! as FFmpeg's `oggparseogm.c` reads them.
//! - The identification header names the media type, the codec (a BMP
//!   FourCC for video, a hexadecimal WAVE format tag for audio, raw text
//!   for subtitles) and the timing: video ticks are `time_unit / (spu *
//!   10^7)` seconds; audio and text run at `spu * 10^7 / time_unit` Hz.
//! - Every data packet starts with a flag byte (keyframe at bit 3) and a
//!   little-endian length field the codec never sees; its value is the
//!   packet's duration.
//! - A page's granule is the start of the packet ending on it
//!   (`granule_is_start`).
//!
//! Codec ids come from the codec registry's FourCC and WAVE-tag claims, so
//! OGM resolves codecs exactly as AVI does.

use oxideav_core::{CodecId, CodecParameters, CodecResolver, CodecTag, MediaType, ProbeContext, TimeBase};

/// The pre-2003 OggDS layout (`ff_ogm_old_codec`).
const OLD_MAGIC: &[u8] = b"\x01Direct Show Samples embedded in Ogg";

/// The number of header packets an OGM stream opening with `first` has
/// (`nb_header`), or `None` when `first` is not an OGM header.
pub(crate) fn header_count(first: &[u8]) -> Option<usize> {
    if first.starts_with(OLD_MAGIC) {
        Some(1)
    } else if first.starts_with(b"\x01video") || first.starts_with(b"\x01audio") || first.starts_with(b"\x01text") {
        Some(2)
    } else {
        None
    }
}

/// Whether a packet in the header phase is a header: FFmpeg ends the
/// headers at the first packet without the low bit.
pub(crate) fn is_header(packet: &[u8]) -> bool {
    packet.first().is_some_and(|b| b & 1 != 0)
}

/// The stream an OGM identification header describes.
pub(crate) struct Header {
    pub(crate) params: CodecParameters,
    pub(crate) time_base: TimeBase,
}

/// FFmpeg's `bytestream2` reads: past the end they return zeros.
struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn skip(&mut self, n: usize) {
        self.at = self.at.saturating_add(n).min(self.data.len());
    }
    fn byte(&self) -> u8 {
        self.data.get(self.at).copied().unwrap_or(0)
    }
    fn bytes<const N: usize>(&mut self) -> [u8; N] {
        let mut out = [0u8; N];
        let end = self.at.saturating_add(N).min(self.data.len());
        out[..end - self.at].copy_from_slice(&self.data[self.at..end]);
        self.at = end;
        out
    }
    fn le16(&mut self) -> u16 {
        u16::from_le_bytes(self.bytes())
    }
    fn le32(&mut self) -> u32 {
        u32::from_le_bytes(self.bytes())
    }
    fn le64(&mut self) -> u64 {
        u64::from_le_bytes(self.bytes())
    }
    fn left(&self) -> usize {
        self.data.len() - self.at
    }
}

/// `avpriv_set_pts_info` with 32-bit `num` / `den`: an invalid base leaves
/// the demuxer's 1 µs default, as FFmpeg keeps its own.
fn pts_info(num: u64, den: u64) -> TimeBase {
    let (num, den) = (num as u32, den as u32);
    if num == 0 || den == 0 {
        TimeBase::new(1, 1_000_000)
    } else {
        TimeBase::new(i64::from(num), i64::from(den))
    }
}

/// The codec the registry claims for `tag`, with the tag kept for a later
/// resolver.
fn tagged(params: &mut CodecParameters, tag: CodecTag, resolver: Option<&dyn CodecResolver>) {
    if let Some(id) = resolver.and_then(|r| r.resolve_tag(&ProbeContext::new(&tag))) {
        params.codec_id = id;
    }
    params.tag = Some(tag);
}

/// `strtol(acid, NULL, 16)` on the four bytes of an OGM audio header:
/// leading space, a sign, an optional `0x`, then hex digits.
fn hex_tag(acid: [u8; 4]) -> u16 {
    let mut s = acid.as_slice();
    while s.first().is_some_and(u8::is_ascii_whitespace) {
        s = &s[1..];
    }
    let negative = s.first() == Some(&b'-');
    if matches!(s.first(), Some(b'-' | b'+')) {
        s = &s[1..];
    }
    if s.len() > 2 && s[0] == b'0' && s[1] | 0x20 == b'x' && s[2].is_ascii_hexdigit() {
        s = &s[2..];
    }
    let value = s.iter().map_while(|&b| char::from(b).to_digit(16)).fold(0i64, |v, d| v * 16 + i64::from(d));
    (if negative { -value } else { value }) as u16
}

/// The stream `first` describes, or `None` where FFmpeg's header parser
/// fails (zero timing, an extradata size past the packet, a truncated
/// DirectShow header).
pub(crate) fn parse_header(first: &[u8], resolver: Option<&dyn CodecResolver>) -> Option<Header> {
    if first.starts_with(OLD_MAGIC) {
        return dshow_header(first, resolver);
    }
    let mut p = Reader { data: first, at: 1 };
    let mut params;
    let video = p.byte() == b'v';
    if video {
        params = CodecParameters::video(CodecId::new("unknown"));
        p.skip(8);
        let tag = p.le32().to_le_bytes();
        tagged(&mut params, CodecTag::fourcc(&tag), resolver);
    } else if p.byte() == b't' {
        params = CodecParameters::subtitle(CodecId::new("text"));
        p.skip(12);
    } else {
        params = CodecParameters::audio(CodecId::new("unknown"));
        p.skip(8);
        let cid = hex_tag(p.bytes());
        tagged(&mut params, CodecTag::wave_format(cid), resolver);
    }
    let mut size = (p.le32() as usize).min(first.len());
    let time_unit = p.le64();
    let spu = p.le64();
    if time_unit == 0 || spu == 0 {
        return None;
    }
    p.skip(4); // default_len
    p.skip(8); // buffersize, bits_per_sample
    let time_base = if video {
        params.width = Some(p.le32());
        params.height = Some(p.le32());
        pts_info(time_unit, spu.wrapping_mul(10_000_000))
    } else {
        // Audio and text alike: FFmpeg reads the audio fields for both,
        // and both run at `spu * 10^7 / time_unit` Hz.
        let channels = p.le16();
        p.skip(2); // block_align
        let bit_rate = u64::from(p.le32()) * 8;
        let rate = (spu.wrapping_mul(10_000_000) / time_unit) as i32;
        if params.media_type == MediaType::Audio {
            params.channels = Some(channels);
            params.bit_rate = Some(bit_rate);
            if rate > 0 {
                params.sample_rate = Some(rate as u32);
            }
        }
        if size >= 56 && params.codec_id.as_str() == "aac" {
            p.skip(4);
            size -= 4;
        }
        if size > 52 {
            size -= 52;
            if p.left() < size {
                return None;
            }
            params.extradata = first[p.at..p.at + size].to_vec();
        }
        pts_info(1, rate.max(0) as u64)
    };
    Some(Header { params, time_base })
}

/// `ogm_dshow_header`: the pre-2003 layout, media type at byte 96.
fn dshow_header(first: &[u8], resolver: Option<&dyn CodecResolver>) -> Option<Header> {
    let le16 = |at: usize| u16::from_le_bytes([first[at], first[at + 1]]);
    let le32 = |at: usize| u32::from_le_bytes([first[at], first[at + 1], first[at + 2], first[at + 3]]);
    if first.len() < 100 {
        return None;
    }
    match le32(96) {
        0x0558_9F80 => {
            if first.len() < 184 {
                return None;
            }
            let mut params = CodecParameters::video(CodecId::new("unknown"));
            tagged(&mut params, CodecTag::fourcc(&le32(68).to_le_bytes()), resolver);
            params.width = Some(le32(176));
            params.height = Some(le32(180));
            let unit = u64::from_le_bytes(first[164..172].try_into().expect("8 bytes"));
            Some(Header { params, time_base: pts_info(unit, 10_000_000) })
        }
        0x0558_9F81 => {
            if first.len() < 136 {
                return None;
            }
            let mut params = CodecParameters::audio(CodecId::new("unknown"));
            tagged(&mut params, CodecTag::wave_format(le16(124)), resolver);
            params.channels = Some(le16(126));
            params.sample_rate = Some(le32(128));
            params.bit_rate = Some(u64::from(le32(132)) * 8);
            Some(Header { params, time_base: TimeBase::new(1, 1_000_000) })
        }
        // FFmpeg leaves an unknown DirectShow type undescribed.
        _ => Some(Header { params: CodecParameters::data(CodecId::new("unknown")), time_base: TimeBase::new(1, 1_000_000) }),
    }
}

/// `ogm_packet`: where the codec's bytes start, the duration the length
/// field holds, and the keyframe bit; `None` for a packet shorter than its
/// length field (FFmpeg's `AVERROR_INVALIDDATA`).
pub(crate) fn packet(data: &[u8]) -> Option<(usize, i64, bool)> {
    let flags = *data.first()?;
    let lb = usize::from(((flags & 2) << 1) | ((flags >> 6) & 3));
    if data.len() < lb + 1 {
        return None;
    }
    let duration = (0..lb).fold(0u64, |sum, i| sum + (u64::from(data[i + 1]) << (i * 8)));
    Some((lb + 1, duration as i64, flags & 8 != 0))
}

/// The Vorbis comment block of an OGM comment header (type 3): FFmpeg skips
/// 7 bytes and drops the last (the framing bit).
pub(crate) fn comment(packet: &[u8]) -> Option<&[u8]> {
    (packet.first() == Some(&3) && packet.len() > 8).then(|| &packet[7..packet.len() - 1])
}
