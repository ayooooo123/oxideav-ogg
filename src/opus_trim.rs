// SPDX-License-Identifier: LGPL-2.1-or-later
// Port of FFmpeg 2da55bf libavformat/oggparseopus.c (opus_duration, the
// timestamps and trims of opus_packet, the pre-skip of opus_header).
// Copyright (c) 2012 Nicolas George
//
// This file is free software; you can redistribute it and/or modify it under
// the terms of the GNU Lesser General Public License as published by the Free
// Software Foundation; either version 2.1, or (at your option) any later version.
// It is distributed WITHOUT ANY WARRANTY; without even the implied warranty
// of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See LICENSE-LGPL.

//! Opus timestamps and trims in Ogg, as FFmpeg's `oggparseopus.c` gives
//! them. A page's packets follow on from the previous page's granule, or
//! from the first data page's granule less its packets' TOC durations.
//! - A packet's pts is the granule it starts at less the OpusHead
//!   pre-skip: the presentation time of its first sample once the pre-skip
//!   is gone (RFC 7845 §4.3), the axis this crate's seeks use too.
//! - The stream's first data packet after its headers skips the pre-skip
//!   (`start_trimming`); a seek does not re-arm it, as FFmpeg's
//!   `ogg_reset` does not. The decoder's own pre-skip is then the
//!   consumer's to replace with this skip.
//! - The packets of the end-of-stream page that end past its granule carry
//!   that excess, at most their own duration, as padding
//!   (`end_trimming`).
//!
//! `end` is where a stream's packets end so far, in 48 kHz samples: `None`
//! until a page places it, and again after a seek.

use oxideav_core::AudioTrim;

/// FFmpeg's Ogg demuxer treats other granules as absent.
fn valid(granule: i64) -> bool {
    (0..=1i64 << 62).contains(&granule)
}

/// A page with `packets` (its completed data packets) begins: without a
/// running end, it comes from this page, unless this is the end-of-stream
/// page, whose granule excludes its padding.
pub(crate) fn begin_page(end: &mut Option<i64>, granule: i64, eos: bool, packets: &[Vec<u8>]) {
    if !packets.is_empty() && end.is_none() && valid(granule) && !eos {
        let total: i64 = packets.iter().filter_map(|p| duration(p)).map(i64::from).sum();
        *end = Some(granule - total);
    }
}

/// The pts and the trim of `packet`, the next data packet of a page with
/// `granule`. `first`: it is the stream's first after its headers, which
/// skips the pre-skip and, without a page to place it, starts at granule 0.
/// A packet nothing places gets no pts and no padding.
pub(crate) fn packet(
    end: &mut Option<i64>,
    first: &mut bool,
    pre_skip: u16,
    packet: &[u8],
    granule: i64,
    eos: bool,
) -> (Option<i64>, Option<AudioTrim>) {
    let duration = i64::from(duration(packet).unwrap_or(0));
    let first = std::mem::take(first);
    let start = end.or(first.then_some(0));
    let pts = start.map(|s| s.saturating_sub(i64::from(pre_skip)));
    *end = start.map(|s| s.saturating_add(duration));
    let skip = if first { u32::from(pre_skip) } else { 0 };
    let padding = match *end {
        Some(packet_end) if eos && valid(granule) => packet_end.saturating_sub(granule).min(duration).max(0),
        _ => 0,
    };
    let trim = (skip > 0 || padding > 0)
        .then_some(AudioTrim { skip_samples: skip, discard_padding: padding as u32, sample_rate: 48_000 });
    (pts, trim)
}

/// A page with data packets ended: its granule is where they end.
pub(crate) fn end_page(end: &mut Option<i64>, granule: i64, had_data: bool) {
    if had_data && valid(granule) {
        *end = Some(granule);
    }
}

/// A packet's duration in 48 kHz samples from its TOC byte (RFC 6716 §3.1,
/// `opus_duration`). `None` for an empty or truncated packet.
pub(crate) fn duration(packet: &[u8]) -> Option<u32> {
    let toc = *packet.first()?;
    let config = u32::from(toc >> 3);
    let frame = match config {
        0..=11 => (960 * (config & 3)).max(480),
        12..=15 => 480 << (config & 1),
        _ => 120 << (config & 3),
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => u32::from(*packet.get(1)? & 0x3F),
    };
    Some(frame * frames)
}
