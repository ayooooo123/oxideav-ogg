use std::fs::File;
use std::io::Cursor;
use std::path::PathBuf;
use std::process::Command;
use oxideav_core::{Demuxer, NullCodecResolver, ReadSeek};
use oxideav_ogg::page::{flags, lace, Page};

const SERIAL: u32 = 0x5EED_0B15;
const OPUS_SERIAL: u32 = 0x0F1E_2D3C;

fn have(cmd: &str, probe_arg: &str) -> bool {
    Command::new(cmd)
        .arg(probe_arg)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

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

fn opus_head() -> Vec<u8> {
    let mut p = b"OpusHead".to_vec();
    p.extend_from_slice(&[1, 2]);
    p.extend_from_slice(&0u16.to_le_bytes()); // pre_skip 0
    p.extend_from_slice(&48_000u32.to_le_bytes());
    p.extend_from_slice(&[0, 0, 0]);
    p
}

fn opus_tags() -> Vec<u8> {
    [&b"OpusTags"[..], &[0; 8]].concat()
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

fn ensure_theora_vorbis_fixture() -> Option<PathBuf> {
    if !have("ffmpeg", "-version") {
        eprintln!("ffmpeg not found; skipping test");
        return None;
    }
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
    Some(path)
}

fn ensure_vorbis_only_fixture() -> Option<PathBuf> {
    if !have("ffmpeg", "-version") {
        eprintln!("ffmpeg not found; skipping test");
        return None;
    }
    let dir = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"))
        .join("fixtures");
    std::fs::create_dir_all(&dir).expect("create fixtures dir");
    let path = dir.join("vorbis_6s.ogg");
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
                "sine=frequency=440:sample_rate=44100",
                "-t",
                "6",
                "-c:a",
                "libvorbis",
            ])
            .arg(&path)
            .status()
            .expect("run ffmpeg");
        assert!(status.success(), "ffmpeg fixture generation succeeded");
    }
    Some(path)
}

fn ffprobe_last_vorbis_packet_duration(path: &PathBuf) -> Option<u32> {
    if !have("ffprobe", "-version") {
        eprintln!("ffprobe not found; skipping");
        return None;
    }
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "packet=pts,duration",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("run ffprobe");
    assert!(output.status.success(), "ffprobe succeeded");
    let text = String::from_utf8(output.stdout).unwrap();
    let last_line = text.lines().rfind(|l| !l.is_empty()).expect("last packet line");
    let mut parts = last_line.split(',');
    let _pts = parts.next().unwrap();
    let dur: u32 = parts.next().unwrap().parse().unwrap();
    Some(dur)
}

fn ffprobe_eos_packet_count(path: &PathBuf) -> Option<usize> {
    if !have("ffprobe", "-version") {
        return None;
    }
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "packet=pos",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("run ffprobe");
    assert!(output.status.success(), "ffprobe succeeded");
    let text = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    let last_pos = lines.last().copied()?.trim_end_matches(',');
    let count = lines.iter().filter(|&&pos| pos.trim_end_matches(',') == last_pos).count();
    Some(count)
}

/// causes all Vorbis packets on that EOS page to be placed at the previous page's granule,
/// matching uninterrupted playback.
#[test]
fn finding_1_seek_into_last_second_vorbis_pts() {
    let Some(path) = ensure_theora_vorbis_fixture() else { return };
    let Some(eos_count) = ffprobe_eos_packet_count(&path) else { return };

    // Straight-through reference: collect all Vorbis packet pts.
    let file = File::open(&path).expect("open file");
    let input: Box<dyn ReadSeek> = Box::new(file);
    let mut d_fresh = oxideav_ogg::demux::open(input, &NullCodecResolver).unwrap();
    let mut straight_through_pts = Vec::new();
    while let Ok(pkt) = d_fresh.next_packet() {
        if pkt.stream_index == 1 {
            straight_through_pts.push(pkt.pts);
        }
    }
    assert!(straight_through_pts.len() >= eos_count);
    let eos_straight_through = &straight_through_pts[straight_through_pts.len() - eos_count..];

    // Seek to video frame 144 (at 5.76s), which lands on video page at 161393.
    // The Vorbis EOS page follows at offset 168351.
    let file = File::open(&path).expect("open file");
    let input: Box<dyn ReadSeek> = Box::new(file);
    let mut d = oxideav_ogg::demux::open(input, &NullCodecResolver).unwrap();
    d.seek_to(0, 144).unwrap();

    let mut vorbis_pts = Vec::new();
    while let Ok(pkt) = d.next_packet() {
        if pkt.stream_index == 1 {
            vorbis_pts.push(pkt.pts);
        }
    }
    assert_eq!(vorbis_pts.len(), eos_straight_through.len(), "must demux all EOS packets");
    assert_eq!(vorbis_pts, eos_straight_through, "EOS packets after seek must match uninterrupted timeline exactly");
}

/// F1: When Vorbis pages are separated by > 1 MiB of other pages, backward lookup
/// exhausts the search window; the EOS page is placed from granule - total, and end trim is kept.
#[test]
fn f1_vorbis_eos_unanchored_when_previous_page_exhausted_window() {
    let mut stream_bytes = Vec::new();

    // Vorbis BOS
    stream_bytes.extend(page_of(SERIAL, flags::FIRST_PAGE, 0, 0, &[id_header()]));
    // Opus BOS
    stream_bytes.extend(page_of(OPUS_SERIAL, flags::FIRST_PAGE, 0, 0, &[opus_head()]));
    // Vorbis headers
    stream_bytes.extend(page_of(SERIAL, 0, 0, 1, &[comment_header(), setup_header()]));
    // Opus headers
    stream_bytes.extend(page_of(OPUS_SERIAL, 0, 0, 1, &[opus_tags()]));

    // Vorbis data page 1: granule 256
    stream_bytes.extend(page_of(SERIAL, 0, 256, 2, &[short(), short()]));

    // Opus filler pages > 1 MiB (18 pages * ~65 KB = ~1.17 MiB > 1 << 20)
    let dummy_packet = vec![0xF8; 254 * 255];
    for i in 2..20 {
        let granule = (i as i64) * 960;
        stream_bytes.extend(page_of(OPUS_SERIAL, 0, granule, i as u32, std::slice::from_ref(&dummy_packet)));
    }

    // Vorbis EOS page: granule 1856 (no end trim: 256 + 576 + 1024 = 1856)
    stream_bytes.extend(page_of(SERIAL, flags::LAST_PAGE, 1856, 3, &[long(false), long(true)]));

    let mut d = oxideav_ogg::demux::open(Box::new(Cursor::new(stream_bytes)), &NullCodecResolver).unwrap();
    let opus_idx = d.streams().iter().find(|s| s.params.codec_id.as_str() == "opus").unwrap().index;
    let vorbis_idx = d.streams().iter().find(|s| s.params.codec_id.as_str() == "vorbis").unwrap().index;

    // Seek onto the Opus page right before the Vorbis EOS page (Opus page 19, granule 19 * 960 = 18240)
    d.seek_to(opus_idx, 19 * 960).unwrap();

    let mut vorbis_pts = Vec::new();
    while let Ok(pkt) = d.next_packet() {
        if pkt.stream_index == vorbis_idx {
            vorbis_pts.push(pkt.pts);
        }
    }
    // With window exhausted, page places from granule 1856 - (576 + 1024) = 256 (not near 0).
    assert_eq!(vorbis_pts, &[Some(256), Some(832)]);
}

/// F1 variant: When previous Vorbis page is within the 1 MiB window, EOS page anchors exactly.
#[test]
fn f1_vorbis_eos_anchored_when_previous_page_within_window() {
    let mut stream_bytes = Vec::new();

    // Vorbis BOS + Opus BOS
    stream_bytes.extend(page_of(SERIAL, flags::FIRST_PAGE, 0, 0, &[id_header()]));
    stream_bytes.extend(page_of(OPUS_SERIAL, flags::FIRST_PAGE, 0, 0, &[opus_head()]));
    stream_bytes.extend(page_of(SERIAL, 0, 0, 1, &[comment_header(), setup_header()]));
    stream_bytes.extend(page_of(OPUS_SERIAL, 0, 0, 1, &[opus_tags()]));

    // Vorbis data page 1: granule 256
    stream_bytes.extend(page_of(SERIAL, 0, 256, 2, &[short(), short()]));

    // 2 small Opus pages (< 1 MiB)
    for i in 2..4 {
        let granule = (i as i64) * 960;
        stream_bytes.extend(page_of(OPUS_SERIAL, 0, granule, i as u32, &[vec![0xF8; 100]]));
    }

    // Vorbis EOS page: granule 1856
    stream_bytes.extend(page_of(SERIAL, flags::LAST_PAGE, 1856, 3, &[long(false), long(true)]));

    let mut d = oxideav_ogg::demux::open(Box::new(Cursor::new(stream_bytes)), &NullCodecResolver).unwrap();
    let opus_idx = d.streams().iter().find(|s| s.params.codec_id.as_str() == "opus").unwrap().index;
    let vorbis_idx = d.streams().iter().find(|s| s.params.codec_id.as_str() == "vorbis").unwrap().index;

    // Seek onto Opus page 3
    d.seek_to(opus_idx, 3 * 960).unwrap();

    let mut vorbis_pts = Vec::new();
    while let Ok(pkt) = d.next_packet() {
        if pkt.stream_index == vorbis_idx {
            vorbis_pts.push(pkt.pts);
        }
    }
    assert_eq!(vorbis_pts, &[Some(256), Some(832)]);
}

/// F2: Seeking to 0 on a healthy file must not bump link count, increment duplicate_serials,
/// or record damage events.
#[test]
fn f2_seek_to_0_does_not_count_as_new_link_or_damage() {
    let Some(path) = ensure_theora_vorbis_fixture() else { return };
    let file = File::open(&path).expect("open file");
    let input: Box<dyn ReadSeek> = Box::new(file);
    let mut d = oxideav_ogg::demux::open_concrete(input, &NullCodecResolver).unwrap();

    let fresh_dup = d.duplicate_serial_count();
    let fresh_damage = d.damage_events().len();
    let fresh_links = d.link_count();
    assert_eq!(fresh_dup, 0);
    assert_eq!(fresh_damage, 0);
    assert_eq!(fresh_links, 1);

    for _ in 0..10 {
        d.next_packet().unwrap();
    }

    d.seek_to(0, 0).unwrap();
    d.next_packet().unwrap();

    assert_eq!(d.duplicate_serial_count(), fresh_dup, "duplicate_serials must not increment after seek_to(0)");
    assert_eq!(d.damage_events().len(), fresh_damage, "damage_events must not record damage after seek_to(0)");
    assert_eq!(d.link_count(), fresh_links, "link_count must not bump after seek_to(0)");
}

/// F3: With a dense index, seeking to 1000 lands without emitting secondary header packets as data.
#[test]
fn f3_seek_to_1000_with_dense_index_emits_audio_data() {
    let Some(path) = ensure_theora_vorbis_fixture() else { return };
    let file = File::open(&path).expect("open file");
    let input: Box<dyn ReadSeek> = Box::new(file);
    let mut d = oxideav_ogg::demux::open_concrete(input, &NullCodecResolver).unwrap();
    d.build_seek_index().unwrap();

    d.seek_to(1, 1000).unwrap();
    let mut pkt_vorbis = None;
    while let Ok(p) = d.next_packet() {
        if p.stream_index == 1 {
            pkt_vorbis = Some(p);
            break;
        }
    }
    let pkt_vorbis = pkt_vorbis.unwrap();
    assert_eq!(pkt_vorbis.stream_index, 1);
    assert_eq!(pkt_vorbis.data[0] & 1, 0, "first packet must be audio data, not header");
}

/// Verify 2: On an EOS page without end trim, unanchored seek pts equal straight-through pts.
#[test]
fn eos_page_without_end_trim_unanchored_pts_equal_straight_through() {
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
    let Some(path) = ensure_theora_vorbis_fixture() else { return };
    if !have("ffprobe", "-version") { return; }

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

    let mut d_fresh = oxideav_ogg::demux::open(Box::new(Cursor::new(stream.clone())), &NullCodecResolver).unwrap();
    let mut fresh_pts = Vec::new();
    while let Ok(p) = d_fresh.next_packet() {
        fresh_pts.push(p.pts);
    }
    assert_eq!(fresh_pts, [-128, 0, 128, 704].map(Some));

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
    let Some(path) = ensure_theora_vorbis_fixture() else { return };
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

/// Last Vorbis packet of a stream carries its end padding as an audio trim,
/// equal to block duration minus ffprobe's last packet duration, both straight-through
/// and after a seek into the last second.
#[test]
fn last_vorbis_packet_carries_end_padding_as_audio_trim() {
    let fixtures = [
        ensure_theora_vorbis_fixture().map(|p| (p, 1u32, true)),
        ensure_vorbis_only_fixture().map(|p| (p, 0u32, false)),
    ];

    for item in fixtures.into_iter().flatten() {
        let (path, vorbis_idx, has_video) = item;
        let Some(ffprobe_dur) = ffprobe_last_vorbis_packet_duration(&path) else { continue };

        // 1. Straight-through: find block duration and check trims
        let file = File::open(&path).expect("open file");
        let input: Box<dyn ReadSeek> = Box::new(file);
        let mut d = oxideav_ogg::demux::open(input, &NullCodecResolver).unwrap();
        let mut trims = Vec::new();
        while let Ok(pkt) = d.next_packet() {
            if pkt.stream_index == vorbis_idx {
                trims.push(d.packet_metadata().audio_trim);
            }
        }
        assert!(trims.len() >= 2);
        let last_trim = trims.pop().unwrap();
        for (i, t) in trims.iter().enumerate() {
            assert!(t.is_none(), "intermediate packet {i} must have no trim");
        }
        let last_trim = last_trim.expect("last Vorbis packet must carry AudioTrim");
        assert_eq!(last_trim.skip_samples, 0);

        // Derive expected discard padding from block duration (1024) minus ffprobe duration
        let block_dur = 1024u32;
        let expected_discard = block_dur.checked_sub(ffprobe_dur).unwrap();
        assert_eq!(last_trim.discard_padding, expected_discard);

        // 2. After a seek into the last second
        let file = File::open(&path).expect("open file");
        let input: Box<dyn ReadSeek> = Box::new(file);
        let mut d = oxideav_ogg::demux::open(input, &NullCodecResolver).unwrap();
        if has_video {
            d.seek_to(0, 144).unwrap(); // seek onto video page
        } else {
            d.seek_to(0, 242550).unwrap(); // seek onto audio
        }
        let mut seek_trims = Vec::new();
        while let Ok(pkt) = d.next_packet() {
            if pkt.stream_index == vorbis_idx {
                seek_trims.push(d.packet_metadata().audio_trim);
            }
        }
        assert!(!seek_trims.is_empty());
        let last_seek_trim = seek_trims.pop().unwrap();
        for (i, t) in seek_trims.iter().enumerate() {
            assert!(t.is_none(), "intermediate seek packet {i} must have no trim");
        }
        let last_seek_trim = last_seek_trim.expect("last Vorbis packet after seek must carry AudioTrim");
        assert_eq!(last_seek_trim.skip_samples, 0);
        assert_eq!(last_seek_trim.discard_padding, expected_discard);
    }
}
