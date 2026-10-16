use std::fs::File;
use std::io::Cursor;
use std::path::PathBuf;
use std::process::Command;
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

fn short() -> Vec<u8> {
    vec![0x00, 0x5A]
}

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

fn ensure_theora_vorbis_fixture() -> PathBuf {
    let dir = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"))
        .join("fixtures");
    std::fs::create_dir_all(&dir).expect("create fixtures dir");
    let path = dir.join("theora_vorbis_6s.ogg");
    if !path.exists() {
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=160x120:rate=25",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=44100",
                "-t",
                "6",
                "-c:v",
                "libtheora",
                "-c:a",
                "libvorbis",
                "-shortest",
            ])
            .arg(&path)
            .status()
            .expect("run ffmpeg");
        assert!(status.success(), "ffmpeg fixture generation succeeded");
    }
    path
}

/// Finding 1: In a multiplexed file, seeking onto a video page before the Vorbis EOS page
/// causes all Vorbis packets on that EOS page to have pts None.
/// After the fix: every Vorbis packet after the seek has a Some(pts).
#[test]
fn finding_1_seek_into_last_second_vorbis_pts() {
    let path = ensure_theora_vorbis_fixture();
    let file = File::open(&path).expect("open file");
    let input: Box<dyn ReadSeek> = Box::new(file);
    let mut d = oxideav_ogg::demux::open(input, &NullCodecResolver).unwrap();

    // Seek to video frame 144 (at 5.76s), which lands on video page at 161393.
    // The Vorbis EOS page follows at offset 168351.
    d.seek_to(0, 144).unwrap();

    let mut vorbis_pts = Vec::new();
    while let Ok(pkt) = d.next_packet() {
        if pkt.stream_index == 1 {
            vorbis_pts.push(pkt.pts);
        }
    }
    assert!(!vorbis_pts.is_empty(), "must have Vorbis packets after seek");
    for (i, pts) in vorbis_pts.iter().enumerate() {
        assert!(pts.is_some(), "packet {i} has None pts after seek into last second");
    }
}

/// Verify 2: On an EOS page without end trim, unanchored seek pts equal straight-through pts.
#[test]
fn eos_page_without_end_trim_unanchored_pts_equal_straight_through() {
    // Page 2 ends at granule 256.
    // Page 3 has long(false) [576 samples] and long(true) [1024 samples], ending at 256 + 1600 = 1856.
    // No end trim: granule exactly equals 256 + 1600 = 1856.
    let stream = [
        page(flags::FIRST_PAGE, 0, 0, &[id_header()]),
        page(0, 0, 1, &[comment_header(), setup_header()]),
        page(0, 256, 2, &[short(), short()]),
        page(flags::LAST_PAGE, 1856, 3, &[long(false), long(true)]),
    ]
    .concat();

    // Straight-through
    let mut d_fresh = oxideav_ogg::demux::open(Box::new(Cursor::new(stream.clone())), &NullCodecResolver).unwrap();
    let mut straight_through = Vec::new();
    while let Ok(p) = d_fresh.next_packet() {
        straight_through.push(p.pts);
    }
    // Page 3 packets are indices 2 and 3: [Some(256), Some(832)]
    let page3_straight_through = &straight_through[2..];
    assert_eq!(page3_straight_through, &[Some(256), Some(832)]);

    // Seek onto page 3:
    let mut d_seek = oxideav_ogg::demux::open(Box::new(Cursor::new(stream)), &NullCodecResolver).unwrap();
    d_seek.seek_to(0, 1900).unwrap();
    let mut seek_pts = Vec::new();
    while let Ok(p) = d_seek.next_packet() {
        seek_pts.push(p.pts);
    }
    assert_eq!(seek_pts, page3_straight_through);
}

/// Verify 2: Compare seek on audio stream with ffprobe output.
#[test]
fn seek_audio_matches_ffprobe_on_eos_page() {
    let path = ensure_theora_vorbis_fixture();

    // Query ffprobe for audio packets around 5.5s:
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a",
            "-read_intervals",
            "5.5%",
            "-show_entries",
            "packet=pts,duration",
            "-of",
            "csv=p=0",
        ])
        .arg(&path)
        .output()
        .expect("run ffprobe");
    assert!(output.status.success(), "ffprobe succeeded");
    let text = String::from_utf8(output.stdout).unwrap();
    let ffprobe_packets: Vec<(i64, i64)> = text
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let mut parts = l.split(',');
            let pts: i64 = parts.next().unwrap().parse().unwrap();
            let dur: i64 = parts.next().unwrap().parse().unwrap();
            (pts, dur)
        })
        .collect();

    // Seek to 5.5s (pts 242550) on stream 1 in oxideav-ogg:
    let file = File::open(&path).expect("open file");
    let input: Box<dyn ReadSeek> = Box::new(file);
    let mut d = oxideav_ogg::demux::open(input, &NullCodecResolver).unwrap();
    d.seek_to(1, 242550).unwrap();

    let mut ogg_packets = Vec::new();
    while let Ok(pkt) = d.next_packet() {
        if pkt.stream_index == 1 {
            ogg_packets.push(pkt.pts.unwrap());
        }
    }

    // ffprobe and oxideav-ogg land on the same Vorbis page at 140176 (pts 179776)
    // and emit the identical packet sequence up to the EOS page:
    assert_eq!(ogg_packets.len(), ffprobe_packets.len());
    for (ogg_pts, (ff_pts, _)) in ogg_packets.iter().zip(&ffprobe_packets) {
        assert_eq!(ogg_pts, ff_pts);
    }
}

/// Finding 2: A seek onto a file whose only Vorbis data page is the EOS page resumes
/// at the header page's granule 0: the first packet is stamped 0 instead of -duration,
/// and the page runs one packet late.
#[test]
fn finding_2_single_data_page_eos_seek_resumes_with_priming() {
    let stream = [
        page(flags::FIRST_PAGE, 0, 0, &[id_header()]),
        page(0, 0, 1, &[comment_header(), setup_header()]),
        page(flags::LAST_PAGE, 1728, 2, &[short(), short(), long(false), long(true)]),
    ]
    .concat();

    // Fresh open
    let mut d_fresh = oxideav_ogg::demux::open(Box::new(Cursor::new(stream.clone())), &NullCodecResolver).unwrap();
    let mut fresh_pts = Vec::new();
    while let Ok(p) = d_fresh.next_packet() {
        fresh_pts.push(p.pts);
    }
    assert_eq!(fresh_pts, [-128, 0, 128, 704].map(Some));

    // Seek onto page 2 (the EOS page)
    let mut d_seek = oxideav_ogg::demux::open(Box::new(Cursor::new(stream)), &NullCodecResolver).unwrap();
    d_seek.seek_to(0, 1800).unwrap();
    let mut seek_pts = Vec::new();
    while let Ok(p) = d_seek.next_packet() {
        seek_pts.push(p.pts);
    }
    assert_eq!(seek_pts, fresh_pts);
}

/// Finding 3: seek_to(_, 0) can land on a secondary header page and emit comment/setup
/// packets as data.
#[test]
fn finding_3_seek_to_0_does_not_emit_header_packets_as_data() {
    let path = ensure_theora_vorbis_fixture();
    let file = File::open(&path).expect("open file");
    let input: Box<dyn ReadSeek> = Box::new(file);
    let mut d_fresh = oxideav_ogg::demux::open_concrete(input, &NullCodecResolver).unwrap();
    let fresh_theora = d_fresh.next_packet().unwrap();
    let mut fresh_vorbis = None;
    while let Ok(p) = d_fresh.next_packet() {
        if p.stream_index == 1 {
            fresh_vorbis = Some(p);
            break;
        }
    }
    let fresh_vorbis = fresh_vorbis.unwrap();

    let file = File::open(&path).expect("open file");
    let input: Box<dyn ReadSeek> = Box::new(file);
    let mut d = oxideav_ogg::demux::open_concrete(input, &NullCodecResolver).unwrap();
    d.build_seek_index().unwrap();

    // Seek to 0 on Vorbis stream
    d.seek_to(1, 0).unwrap();
    let mut pkt_vorbis = None;
    while let Ok(p) = d.next_packet() {
        if p.stream_index == 1 {
            pkt_vorbis = Some(p);
            break;
        }
    }
    let pkt_vorbis = pkt_vorbis.unwrap();
    assert_eq!(pkt_vorbis.stream_index, 1);
    assert_eq!(pkt_vorbis.pts, fresh_vorbis.pts);
    assert_eq!(pkt_vorbis.data, fresh_vorbis.data);

    // Seek to 0 on Theora stream
    d.seek_to(0, 0).unwrap();
    let mut pkt_theora = None;
    while let Ok(p) = d.next_packet() {
        if p.stream_index == 0 {
            pkt_theora = Some(p);
            break;
        }
    }
    let pkt_theora = pkt_theora.unwrap();
    assert_eq!(pkt_theora.stream_index, 0);
    assert_eq!(pkt_theora.pts, fresh_theora.pts);
    assert_eq!(pkt_theora.data, fresh_theora.data);
}
