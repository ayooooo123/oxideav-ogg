//! Ogg Vorbis timestamps as FFmpeg 2da55bf gives them: every packet's pts
//! is where the samples it completes start, each packet completing a
//! quarter of the block size before it plus a quarter of its own
//! (`vorbis_parser.c`). The stream below has short (256) and long (2048)
//! blocks:
//! - first data page: S S L L, granule 1728. The first packet primes the
//!   decoder, 128 samples before 0; the others start at 0, 128 and 704;
//! - second page: L S S, 1024 + 576 + 128 samples, granule 3456;
//! - end-of-stream page: L L, 576 + 1024 samples, granule 4556 (500 of
//!   padding).

use std::io::Cursor;

use oxideav_core::{Demuxer, NullCodecResolver, ReadSeek};
use oxideav_ogg::page::{flags, lace, Page};

const SERIAL: u32 = 0x5EED_0B15;

/// Bits packed from each value's low bit up (Vorbis I §2.1.4).
struct Bits {
    bytes: Vec<u8>,
    at: usize,
}

impl Bits {
    fn put(&mut self, value: u32, n: usize) {
        for i in 0..n {
            if self.at % 8 == 0 {
                self.bytes.push(0);
            }
            if value >> i & 1 == 1 {
                *self.bytes.last_mut().unwrap() |= 1 << (self.at % 8);
            }
            self.at += 1;
        }
    }
}

/// 48 kHz stereo, blocks of 256 and 2048 samples.
fn id_header() -> Vec<u8> {
    let mut p = b"\x01vorbis".to_vec();
    p.extend_from_slice(&0u32.to_le_bytes());
    p.push(2);
    p.extend_from_slice(&48_000u32.to_le_bytes());
    p.extend_from_slice(&[0; 12]);
    p.extend_from_slice(&[0xB8, 1]);
    p
}

fn comment_header() -> Vec<u8> {
    [&b"\x03vorbis"[..], &[0; 8], &[1]].concat()
}

/// Mode 0 has short blocks, mode 1 long ones. Ones stand in for the
/// codebooks, floors, residues and mappings before the modes, which the
/// demuxer reads past from the end.
fn setup_header() -> Vec<u8> {
    let mut b = Bits {
        bytes: b"\x05vorbis".to_vec(),
        at: 56,
    };
    b.put(u32::MAX, 32);
    b.put(u32::MAX, 32);
    b.put(1, 6); // two modes
    for blockflag in [0, 1] {
        b.put(blockflag, 1);
        b.put(0, 16); // window type
        b.put(0, 16); // transform type
        b.put(0, 8); // mapping
    }
    b.put(1, 1); // framing
    b.bytes
}

/// An audio packet: type bit 0, then the mode bit.
fn short() -> Vec<u8> {
    vec![0x00, 0x5A]
}

/// A long block also says whether the block before it was long.
fn long(after_long: bool) -> Vec<u8> {
    vec![0x02 | u8::from(after_long) << 2, 0x5A]
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

fn open() -> Box<dyn Demuxer> {
    let stream = [
        page(flags::FIRST_PAGE, 0, 0, &[id_header()]),
        page(0, 0, 1, &[comment_header(), setup_header()]),
        page(0, 1728, 2, &[short(), short(), long(false), long(true)]),
        page(0, 3456, 3, &[long(true), short(), short()]),
        page(flags::LAST_PAGE, 4556, 4, &[long(false), long(true)]),
    ]
    .concat();
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(stream));
    oxideav_ogg::demux::open(input, &NullCodecResolver).unwrap()
}

fn pts(d: &mut dyn Demuxer) -> Vec<Option<i64>> {
    let mut out = Vec::new();
    while let Ok(p) = d.next_packet() {
        assert_eq!(p.dts, p.pts);
        out.push(p.pts);
    }
    out
}

#[test]
fn packets_start_where_their_samples_start() {
    let mut d = open();
    assert_eq!(
        pts(&mut *d),
        [-128, 0, 128, 704, 1728, 2752, 3328, 3456, 4032].map(Some)
    );
}

/// A seek lands on the last page ending before the target, so the decoder
/// has the packet before the target's.
#[test]
fn a_seek_places_the_landing_page_from_its_granule() {
    // 4000 lands on the second page: its granule less its packets' 1728.
    let mut d = open();
    d.seek_to(0, 4000).unwrap();
    assert_eq!(pts(&mut *d), [1728, 2752, 3328, 3456, 4032].map(Some));
}

#[test]
fn a_seek_onto_the_end_of_stream_page_starts_at_the_granule_before() {
    // Past the end lands on the end-of-stream page, whose granule leaves
    // out the padding: its packets start where the second page ends.
    let mut d = open();
    d.seek_to(0, 4600).unwrap();
    assert_eq!(pts(&mut *d), [3456, 4032].map(Some));
}
