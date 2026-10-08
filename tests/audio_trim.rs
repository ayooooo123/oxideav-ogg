//! Ogg Opus as FFmpeg 2da55bf's oggparseopus.c reads it, for gapless
//! playback: every packet's pts is the granule it starts at less the
//! OpusHead pre-skip; `Demuxer::packet_metadata().audio_trim` makes the
//! first data packet skip the pre-skip (`start_trimming`) and gives the
//! packets of the end-of-stream page that end past its granule the excess
//! as padding (`end_trimming`); and the stream declares the 48 kHz rate
//! Opus decodes at.

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
    page_of(SERIAL, flags_byte, granule, seq, packets)
}

fn page_of(serial: u32, flags_byte: u8, granule: i64, seq: u32, packets: &[Vec<u8>]) -> Vec<u8> {
    Page {
        flags: flags_byte,
        granule_position: granule,
        serial,
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

/// The first packet's trim: the 312-sample pre-skip.
fn pre_skip() -> Option<AudioTrim> {
    Some(AudioTrim { skip_samples: 312, discard_padding: 0, sample_rate: 48_000 })
}

#[test]
fn the_last_packet_carries_the_padding_past_the_final_granule() {
    // The last two packets end at 3192 + 960 and 3192 + 1920; the stream
    // ends at 4512, so 600 samples of the last are padding.
    assert_eq!(trims(&mut *open(stream(4512))), [pre_skip(), None, None, None, padding(600)]);
}

#[test]
fn padding_longer_than_a_packet_covers_the_ones_before() {
    // Ending at 4000: 152 samples of the fourth packet, all of the fifth.
    assert_eq!(trims(&mut *open(stream(4000))), [pre_skip(), None, None, padding(152), padding(960)]);
}

#[test]
fn packets_start_at_their_granule_less_the_pre_skip() {
    // The first page ends at 3192 after three 960-sample packets: they start
    // at 312, 1272 and 2232, so at 0, 960 and 1920 once the pre-skip is gone.
    let mut d = open(stream(4512));
    let mut pts = Vec::new();
    while let Ok(p) = d.next_packet() {
        assert_eq!(p.dts, p.pts);
        pts.push(p.pts);
    }
    assert_eq!(pts, [Some(0), Some(960), Some(1920), Some(2880), Some(3840)]);
}

#[test]
fn a_seek_counts_the_packets_from_the_landing_page_again() {
    let mut d = open(stream(4512));
    for _ in 0..5 { d.next_packet().unwrap(); }
    assert_eq!(d.packet_metadata().audio_trim, padding(600));
    d.seek_to(0, 0).unwrap();
    assert_eq!(d.packet_metadata().audio_trim, None, "cleared by the seek");
    // The start of the file: the headers are read again, and an OpusHead
    // re-arms the pre-skip, as FFmpeg's `opus_packet` does.
    assert_eq!(trims(&mut *d), [pre_skip(), None, None, None, padding(600)]);
}

#[test]
fn a_seek_into_the_data_drops_the_pending_pre_skip_as_ffmpeg_does() {
    // `ogg_reset`: a seek landing past the headers, here on the
    // end-of-stream page before any packet was read, skips nothing; the
    // packets' pts say where the presentation is.
    let mut d = open(stream(4512));
    d.seek_to(0, 4200).unwrap();
    let mut got = Vec::new();
    while let Ok(p) = d.next_packet() {
        got.push((p.pts, d.packet_metadata().audio_trim));
    }
    assert_eq!(got, [(Some(2880), None), (Some(3840), padding(600))]);
}

#[test]
fn opus_streams_declare_the_48khz_output_rate() {
    // OpusHead's input sample rate (44.1 kHz here) is informational.
    let d = open(stream_from(44_100, 4512));
    assert_eq!(d.streams()[0].params.sample_rate, Some(48_000));
}

#[test]
fn seeking_onto_the_eos_page_keeps_its_padding() {
    let mut d = open(stream(4512));
    for _ in 0..5 { d.next_packet().unwrap(); }
    assert_eq!(d.seek_to(0, 4200).unwrap(), 4512);
    assert_eq!(d.packet_metadata().audio_trim, None);
    assert_eq!(trims(&mut *d), [None, padding(600)]);
}

/// A chained link of Opus after a one-stream Opus link continues that
/// stream, as FFmpeg's `ogg_replace_stream` does: its packets are stream 0's,
/// the first goes out where the first link's sound ended (FFmpeg's running
/// timestamp) and the rest keep their spacing, and its pre-skip and end
/// padding are its own.
#[test]
fn a_chained_opus_link_continues_the_stream() {
    let link = SERIAL + 1;
    let second = [
        page_of(link, flags::FIRST_PAGE, 0, 0, &[opus_head(100, 48_000)]),
        page_of(link, 0, 0, 1, &[opus_tags()]),
        page_of(link, 0, 100 + 2880, 2, &[frame(), frame(), frame()]),
        page_of(link, flags::LAST_PAGE, 4300, 3, &[frame(), frame()]),
    ]
    .concat();
    let mut d = open([stream(4512), second].concat());
    let mut got = Vec::new();
    loop {
        match d.next_packet() {
            Ok(p) => got.push((p.stream_index, p.pts, d.packet_metadata().audio_trim)),
            Err(Error::Eof) => break,
            Err(e) => panic!("demux: {e}"),
        }
    }
    let skip = |n| Some(AudioTrim { skip_samples: n, discard_padding: 0, sample_rate: 48_000 });
    assert_eq!(
        got,
        [
            (0, Some(0), skip(312)),
            (0, Some(960), None),
            (0, Some(1920), None),
            (0, Some(2880), None),
            (0, Some(3840), padding(600)),
            (0, Some(4200), skip(100)),
            (0, Some(5160), None),
            (0, Some(6120), None),
            (0, Some(7080), None),
            (0, Some(8040), padding(600)),
        ]
    );
    assert_eq!(d.streams().len(), 1);
}
