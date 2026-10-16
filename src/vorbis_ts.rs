// SPDX-License-Identifier: LGPL-2.1-or-later
// Port of FFmpeg 2da55bf libavcodec/vorbis_parser.c (parse_id_header,
// parse_setup_header, av_vorbis_parse_frame_flags) and of the Vorbis
// timestamps libavformat/oggdec.c and oggparsevorbis.c give packets.
// Copyright (c) 2012 Justin Ruggles
//
// This file is free software; you can redistribute it and/or modify it under
// the terms of the GNU Lesser General Public License as published by the Free
// Software Foundation; either version 2.1, or (at your option) any later version.
// It is distributed WITHOUT ANY WARRANTY; without even the implied warranty
// of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See LICENSE-LGPL.

//! Vorbis timestamps in Ogg, as FFmpeg gives them: a packet's pts is where
//! the samples it completes start. A packet completes the samples from the
//! centre of the previous packet's window to the centre of its own, a
//! quarter of each block size (Vorbis I §4.3.8). The stream's first packet
//! completes none; FFmpeg stamps it that long before 0 (`ts_offset`), so the
//! samples of the second start at 0.
//! - A page's granule is where the samples of the last packet ending on it
//!   end, so the packets of the next page follow on from it.
//! - With no granule to follow on from (the first data page, and after a
//!   seek), a page's packets start at its granule less their durations.
//!   On the stream's first page, an end-of-stream granule leaves out the
//!   padding of its last packet, so that page places nothing: the
//!   stream's first packet then starts as above. After a seek, if there is
//!   a previous page granule ([`VorbisTs::seek`], or lazily looked up for
//!   an unanchored end-of-stream page), packets start there, matching the
//!   uninterrupted timeline without padding shift.
//! - A zero granule places nothing either: it is the first page's, holding
//!   the priming packet alone, or a broken file's, which FFmpeg distrusts.
//!
//! A short block does not say how long the block before it was, so when a
//! page placed from its granule starts with a short block after a long one,
//! that first packet's own start is off; the packets after it start right.
//! After a seek that packet only primes the decoder.

/// A Vorbis stream's block sizes and modes, and where its packets start.
#[derive(Debug, Clone)]
pub(crate) struct VorbisTs {
    /// The short and the long block size.
    blocksize: [u32; 2],
    /// Per mode: its blocks are long.
    long: Vec<bool>,
    /// The mode number's bits in a packet's first byte.
    mode_mask: u8,
    /// The bit after them: a long block's previous block was long.
    prev_mask: u8,
    /// The block size of the packet before the next one.
    previous: u32,
    /// Where the next data packet starts: `None` until a page places it,
    /// and again after a seek.
    end: Option<i64>,
    /// The next data packet is the stream's first.
    first: bool,
    /// Backward lookup for an end-of-stream page exhausted the search window.
    exhausted: bool,
}

impl VorbisTs {
    /// From the identification and setup headers. `None` when they do not
    /// parse, where FFmpeg refuses the stream.
    pub(crate) fn new(id: &[u8], setup: &[u8]) -> Option<Self> {
        if id.len() < 30 || id[0] != 1 || &id[1..7] != b"vorbis" || id[29] & 1 == 0 {
            return None;
        }
        let blocksize = [1u32 << (id[28] & 0xF), 1u32 << (id[28] >> 4)];
        if setup.len() < 7 || setup[0] != 5 || &setup[1..7] != b"vorbis" {
            return None;
        }
        // The modes end the setup header, behind fields of every length:
        // FFmpeg finds them from the framing bit back, as many as the 6-bit
        // count before them says (mapping < 64, window and transform types
        // 0, a block flag each).
        let mut back = Backwards::new(setup, 0);
        let framed = loop {
            if back.left() <= 97 {
                return None;
            }
            if back.bits(1) == 1 {
                break back.at;
            }
        };
        let (mut count, mut modes) = (0, None);
        while back.left() >= 97 {
            if back.bits(8) > 63 || back.bits(16) != 0 || back.bits(16) != 0 {
                break;
            }
            back.skip(1);
            count += 1;
            if count > 64 {
                break;
            }
            if back.peek(6) + 1 == count {
                modes = Some(count);
            }
        }
        // Up to 63 modes, so the previous-window flag is in the first byte.
        let modes = modes.filter(|&m| m <= 63)?;
        let mut back = Backwards::new(setup, framed);
        let mut long = vec![false; modes as usize];
        for mode in long.iter_mut().rev() {
            back.skip(40);
            *mode = back.bits(1) == 1;
        }
        let mode_bits = (u32::BITS - (modes - 1).leading_zeros()).max(1);
        let mode_mask = (((1u32 << mode_bits) - 1) << 1) as u8;
        Some(Self {
            blocksize,
            long,
            mode_mask,
            prev_mask: (mode_mask | 1) + 1,
            previous: blocksize[0],
            end: None,
            first: true,
            exhausted: false,
        })
    }

    /// How many samples `packet` completes, after a packet with blocks of
    /// `previous` samples, which becomes its own. `None` for a header,
    /// empty or invalid packet, which leaves `previous` alone.
    fn duration(&self, previous: &mut u32, packet: &[u8]) -> Option<u32> {
        let &byte = packet.first()?;
        if byte & 1 == 1 {
            return None;
        }
        let mode = if self.long.len() == 1 {
            0
        } else {
            usize::from((byte & self.mode_mask) >> 1)
        };
        let long = *self.long.get(mode)?;
        let before = if long {
            self.blocksize[usize::from(byte & self.prev_mask != 0)]
        } else {
            *previous
        };
        let current = self.blocksize[usize::from(long)];
        *previous = current;
        Some((before + current) / 4)
    }

    /// A page with `packets` (its completed data packets) begins: without a
    /// running end, its granule places them.
    pub(crate) fn begin_page(&mut self, granule: i64, eos: bool, packets: &[Vec<u8>]) {
        if packets.is_empty() || self.end.is_some() || (eos && self.first) || !places(granule) {
            return;
        }
        let mut previous = self.previous;
        let total: i64 = packets
            .iter()
            .filter_map(|p| self.duration(&mut previous, p))
            .map(i64::from)
            .sum();
        self.end = Some(granule - total);
    }

    /// The pts and optional end padding of `packet`, the next data packet.
    /// `page_granule` and `is_eos_packet` (true only for the last packet completing
    /// on an EOS page) determine end padding: pts + duration - granule when positive.
    pub(crate) fn packet(
        &mut self,
        packet: &[u8],
        page_granule: i64,
        is_eos_packet: bool,
    ) -> (Option<i64>, Option<u32>) {
        let mut previous = self.previous;
        let duration = i64::from(self.duration(&mut previous, packet).unwrap_or(0));
        self.previous = previous;
        let first = std::mem::take(&mut self.first);
        let start = self.end.or(first.then_some(-duration));
        self.end = start.map(|s| s.saturating_add(duration));
        let padding = if is_eos_packet && places(page_granule) {
            start.and_then(|pts| {
                let diff = pts + duration - page_granule;
                (diff > 0).then_some(u32::try_from(diff).unwrap_or(u32::MAX))
            })
        } else {
            None
        };
        (start, padding)
    }

    /// A page with data packets ended: its granule is where they end.
    pub(crate) fn end_page(&mut self, granule: i64, had_data: bool) {
        if had_data && places(granule) {
            self.end = Some(granule);
        }
    }

    /// A seek: the packets read next start at `resume` (where those of an
    /// end-of-stream page start), else where their page places them.
    pub(crate) fn seek(&mut self, resume: Option<PreviousGranule>) {
        match resume {
            Some(PreviousGranule::Found(g)) => {
                self.first = false;
                self.end = Some(g);
                self.exhausted = false;
            }
            Some(PreviousGranule::StreamStart) => {
                self.first = true;
                self.end = None;
                self.exhausted = false;
            }
            Some(PreviousGranule::Exhausted) => {
                self.first = false;
                self.end = None;
                self.exhausted = true;
            }
            None => {
                self.first = false;
                self.end = None;
                self.exhausted = false;
            }
        }
        self.previous = self.blocksize[0];
    }

    /// Whether this stream currently has no anchor for its timeline.
    pub(crate) fn is_unanchored(&self) -> bool {
        self.end.is_none() && !self.first && !self.exhausted
    }

    /// Anchor an end-of-stream page at `resume`, the granule of the stream's
    /// previous page.
    pub(crate) fn anchor_eos(&mut self, resume: PreviousGranule) {
        match resume {
            PreviousGranule::Found(g) => {
                self.first = false;
                self.end = Some(g);
                self.exhausted = false;
            }
            PreviousGranule::StreamStart => {
                self.first = true;
                self.end = None;
                self.exhausted = false;
            }
            PreviousGranule::Exhausted => {
                self.first = false;
                self.end = None;
                self.exhausted = true;
            }
        }
    }
}

/// The outcome of looking up the granule of a stream's previous page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreviousGranule {
    /// A previous data page was found with this non-zero granule.
    Found(i64),
    /// Reached the start of the stream (no earlier data page; granule 0 or start of file).
    StreamStart,
    /// The backward search window was exhausted before reaching the stream's start.
    Exhausted,
}

/// A granule that places packets: FFmpeg's Ogg demuxer treats negative and
/// larger ones as absent, and a zero one is no position (see the module).
fn places(granule: i64) -> bool {
    (1..=1i64 << 62).contains(&granule)
}

/// Reads a setup header from its last bit backwards. Vorbis packs each
/// field from its low bit up (§2.1.4), so read this way a field comes out
/// whole, high bit first, as FFmpeg reads the byte-reversed header.
struct Backwards<'a> {
    data: &'a [u8],
    /// Bits read, from the end.
    at: usize,
}

impl<'a> Backwards<'a> {
    fn new(data: &'a [u8], at: usize) -> Self {
        Self { data, at }
    }

    fn left(&self) -> usize {
        self.data.len() * 8 - self.at
    }

    fn skip(&mut self, n: usize) {
        self.at += n;
    }

    fn bits(&mut self, n: usize) -> u32 {
        (0..n).fold(0, |value, _| {
            let byte = self.data[self.data.len() - 1 - self.at / 8];
            let bit = byte >> (7 - self.at % 8) & 1;
            self.at += 1;
            value << 1 | u32::from(bit)
        })
    }

    /// The next `n` bits, left unread.
    fn peek(&self, n: usize) -> u32 {
        Backwards::new(self.data, self.at).bits(n)
    }
}
