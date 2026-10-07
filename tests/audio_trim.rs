//! Ogg Opus as FFmpeg 2da55bf's oggparseopus.c reads it, for gapless
//! playback: `Demuxer::packet_metadata().audio_trim` gives the packets of
//! the end-of-stream page that end past its granule the excess as padding
//! (`end_trimming`), and the stream declares the 48 kHz rate Opus decodes
//! at. (The decoder applies the OpusHead pre-skip itself.)

use std::io::Cursor;

use oxideav_core::{AudioTrim, Demuxer, Error, NullCodecResolver, ReadSeek};
use oxideav_ogg::page::{flags, lace, Page};

const SERIAL: u32 = 0x0F1E_2D3C;

fn opus_head(pre_skip: u16, input_rate: u32) -> Vec<u8> {
    let mut p = b"OpusHead".to_vec();
    p.extend_from_slice(&[1, 2]);
    p.extend_from_slice(&pre_skip.to_le_bytes());
    p.extend_from_slice(&input_rate.to_le_bytes());
    p.extend_from_slice(&[0, 0, 0]);
    p
}

fn opus_tags() -> Vec<u8> {
    [&b"OpusTags"[..], &[0; 8]].concat()
}

/// A 20 ms CELT fullband packet (TOC config 31, one frame: 960 samples).
fn frame() -> Vec<u8> {
    vec![0xF8, 0xFF, 0xFE]
}

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

/// OpusHead (pre-skip 312, `input_rate`), OpusTags, a page of three
/// 960-sample packets (granule 312 + 2880), and an end-of-stream page of two
/// more ending at `last_granule`.
fn stream_from(input_rate: u32, last_granule: i64) -> Vec<u8> {
    [
        page(flags::FIRST_PAGE, 0, 0, &[opus_head(312, input_rate)]),
        page(0, 0, 1, &[opus_tags()]),
        page(0, 312 + 2880, 2, &[frame(), frame(), frame()]),
        page(flags::LAST_PAGE, last_granule, 3, &[frame(), frame()]),
    ]
    .concat()
}

fn stream(last_granule: i64) -> Vec<u8> {
    stream_from(48_000, last_granule)
}

fn open(bytes: Vec<u8>) -> Box<dyn Demuxer> {
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_ogg::demux::open(input, &NullCodecResolver).unwrap()
}

fn trims(d: &mut dyn Demuxer) -> Vec<Option<AudioTrim>> {
    let mut out = Vec::new();
    loop {
        match d.next_packet() {
            Ok(_) => out.push(d.packet_metadata().audio_trim),
            Err(Error::Eof) => return out,
            Err(e) => panic!("demux: {e}"),
        }
    }
}

fn padding(discard: u32) -> Option<AudioTrim> {
    Some(AudioTrim { skip_samples: 0, discard_padding: discard, sample_rate: 48_000 })
}

#[test]
fn the_last_packet_carries_the_padding_past_the_final_granule() {
    // The last two packets end at 3192 + 960 and 3192 + 1920; the stream
    // ends at 4512, so 600 samples of the last are padding.
    assert_eq!(trims(&mut *open(stream(4512))), [None, None, None, None, padding(600)]);
}

#[test]
fn padding_longer_than_a_packet_covers_the_ones_before() {
    // Ending at 4000: 152 samples of the fourth packet, all of the fifth.
    assert_eq!(trims(&mut *open(stream(4000))), [None, None, None, padding(152), padding(960)]);
}

#[test]
fn a_seek_counts_the_packets_from_the_landing_page_again() {
    let mut d = open(stream(4512));
    assert_eq!(trims(&mut *d).last(), Some(&padding(600)));
    d.seek_to(0, 0).unwrap();
    assert_eq!(d.packet_metadata().audio_trim, None, "cleared by the seek");
    assert_eq!(trims(&mut *d), [None, None, None, None, padding(600)]);
}

#[test]
fn opus_streams_declare_the_48khz_output_rate() {
    // OpusHead's input sample rate (44.1 kHz here) is informational.
    let d = open(stream_from(44_100, 4512));
    assert_eq!(d.streams()[0].params.sample_rate, Some(48_000));
}
