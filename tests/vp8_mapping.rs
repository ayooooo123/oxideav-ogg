//! VP8 in Ogg, read as FFmpeg 2da55bf's oggparsevp8.c reads it: an `OVP80`
//! stream-info header (size, frame rate: the time base) and an optional
//! comment header; a page's granule holds the pts after its last visible
//! frame, so its first packet starts that many visible frames earlier, and
//! each packet lasts its frame header's show bit (an invisible altref frame
//! takes no time). A frame header's key bit makes a keyframe.

use std::io::Cursor;

use oxideav_core::{Error, MediaType, NullCodecResolver, ReadSeek, TimeBase};
use oxideav_ogg::page::{flags, lace, Page};

const SERIAL: u32 = 0x0B08;

fn page(flags_byte: u8, granule: i64, seq: u32, packets: &[Vec<u8>]) -> Vec<u8> {
    Page {
        flags: flags_byte,
        granule_position: granule,
        serial: SERIAL,
        seq_no: seq,
        lacing: packets.iter().flat_map(|p| lace(p.len())).collect(),
        data: packets.concat(),
    }
    .to_bytes()
}

/// The stream-info header: version 1.0, 64x48, square pixels, 25 fps.
fn stream_info() -> Vec<u8> {
    let mut p = b"OVP80\x01\x01\x00".to_vec();
    p.extend_from_slice(&64u16.to_be_bytes());
    p.extend_from_slice(&48u16.to_be_bytes());
    p.extend_from_slice(&[0, 0, 1, 0, 0, 1]);
    p.extend_from_slice(&25u32.to_be_bytes());
    p.extend_from_slice(&1u32.to_be_bytes());
    p
}

/// A VP8 frame starting with its frame tag: bit 0 clear for a keyframe,
/// bit 4 set when the frame is shown.
fn frame(key: bool, shown: bool) -> Vec<u8> {
    vec![u8::from(!key) | (u8::from(shown) << 4), 0, 0]
}

/// The granule of a page whose last frame is visible: the pts after it,
/// and the frames since the keyframe.
fn granule(pts: i64, since_key: i64) -> i64 {
    (pts << 32) | (3 << 30) | (since_key << 3)
}

#[test]
fn vp8_streams_and_packets_are_ffmpegs() {
    let file = [
        page(flags::FIRST_PAGE, 0, 0, &[stream_info()]),
        page(0, 0, 1, &[[b"OVP80\x02\x20".as_slice(), &[0; 8]].concat()]),
        page(0, granule(3, 3), 2, &[frame(true, true), frame(false, true), frame(false, false), frame(false, true)]),
        page(flags::LAST_PAGE, granule(5, 5), 3, &[frame(false, true), frame(false, true)]),
    ]
    .concat();
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(file));
    let mut d = oxideav_ogg::demux::open(input, &NullCodecResolver).expect("open");
    let stream = d.streams()[0].clone();
    assert_eq!(stream.params.media_type, MediaType::Video);
    assert_eq!(stream.params.codec_id.as_str(), "vp8");
    assert_eq!((stream.params.width, stream.params.height), (Some(64), Some(48)));
    assert_eq!(stream.time_base, TimeBase::new(1, 25));
    let mut got = Vec::new();
    loop {
        match d.next_packet() {
            Ok(p) => got.push((p.pts, p.flags.keyframe)),
            Err(Error::Eof) => break,
            Err(e) => panic!("demux: {e}"),
        }
    }
    // The altref frame shares the next frame's pts.
    assert_eq!(
        got,
        [(Some(0), true), (Some(1), false), (Some(2), false), (Some(2), false), (Some(3), false), (Some(4), false)]
    );
}
