// SPDX-License-Identifier: LGPL-2.1-or-later
// Port of FFmpeg 2da55bf libavformat/oggparsevp8.c (vp8_header,
// vp8_gptopts, the timestamps of vp8_packet).
// Copyright (C) 2013 James Almer
//
// This file is free software; you can redistribute it and/or modify it under
// the terms of the GNU Lesser General Public License as published by the Free
// Software Foundation; either version 2.1, or (at your option) any later version.
// It is distributed WITHOUT ANY WARRANTY; without even the implied warranty
// of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See LICENSE-LGPL.

//! VP8 in Ogg (the `OVP80` mapping), as FFmpeg reads it.
//!
//! - Headers start with `0x4F` (`OVP80`, then the type): the stream-info
//!   header (type 1, version 1: size and frame rate, whose inverse is the
//!   time base) and an optional comment header (type 2). The first packet
//!   that does not start so is a frame.
//! - A page's granule holds, in its upper 32 bits, the pts after its last
//!   frame (less one when that frame is invisible); bits 3 to 29 count the
//!   frames since the keyframe.
//! - A page's first packet starts as many shown frames before that pts as
//!   the page holds; each packet lasts one frame if its frame header's show
//!   bit is set, else none (an altref frame). On the end-of-stream page the
//!   count runs on from the page before.

/// The bytes every header starts with, before its type.
const MAGIC: &[u8] = b"OVP80";

/// A packet of the mapping's headers (`vp8_header`'s test).
pub(crate) fn is_header(packet: &[u8]) -> bool {
    packet.len() >= 7 && packet[0] == 0x4F
}

/// A stream-info header's picture size and frame rate (`num`, `den`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StreamInfo {
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) frame_rate: (u32, u32),
}

/// The stream-info header (type 1, version 1, 26 bytes), as `vp8_header`
/// reads it.
pub(crate) fn stream_info(packet: &[u8]) -> Option<StreamInfo> {
    if packet.len() < 26 || !packet.starts_with(MAGIC) || packet[5] != 1 || packet[6] != 1 {
        return None;
    }
    let be32 = |at: usize| u32::from_be_bytes([packet[at], packet[at + 1], packet[at + 2], packet[at + 3]]);
    Some(StreamInfo {
        width: u16::from_be_bytes([packet[8], packet[9]]),
        height: u16::from_be_bytes([packet[10], packet[11]]),
        frame_rate: (be32(18), be32(22)),
    })
}

/// The pts a page's granule names (`vp8_gptopts`).
pub(crate) fn granule_pts(granule: i64) -> i64 {
    let granule = granule as u64;
    let invisible = (granule >> 30) & 3 == 0;
    ((granule >> 32) as i64).saturating_sub(i64::from(invisible))
}

/// The frames a packet lasts: its frame header's show bit.
pub(crate) fn duration(packet: &[u8]) -> i64 {
    packet.first().map_or(0, |b| i64::from((b >> 4) & 1))
}

/// A frame whose header's key-frame bit (bit 0, clear) marks a keyframe.
pub(crate) fn is_keyframe(packet: &[u8]) -> bool {
    packet.first().is_some_and(|b| b & 1 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn granules_name_the_pts_after_the_last_frame() {
        // Visible last frame: the pts as written.
        assert_eq!(granule_pts((4 << 32) | (3 << 30) | (3 << 3)), 4);
        // Invisible last frame: one less.
        assert_eq!(granule_pts((4 << 32) | (3 << 3)), 3);
    }
}
