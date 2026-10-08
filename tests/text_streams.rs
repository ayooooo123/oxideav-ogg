//! Timed-text and OGM logical streams.
//!
//! * Kate (https://wiki.xiph.org/OggKate) and CMML
//!   (https://wiki.xiph.org/CMML) are subtitle streams. Their time base is
//!   the granule rate in the identification header; a packet's pts is the
//!   time its page's granule names: Kate's base plus offset, CMML's upper
//!   bits. Kate declares its header count in the ID header (byte 11); CMML
//!   has three header packets.
//! * OGM streams are read as FFmpeg 2da55bf's `oggparseogm.c` reads them:
//!   the stream header gives the media type, codec tag, size and time base;
//!   every data packet drops its flag byte and length field, whose value is
//!   the packet's duration; a page's granule is the start of the packet
//!   ending on it; and only packets with the low bit set are headers.

use std::io::Cursor;

use oxideav_core::{CodecTag, Demuxer, Error, MediaType, NullCodecResolver, Packet, ReadSeek, TimeBase};
use oxideav_ogg::page::{flags, lace, Page};

fn page(serial: u32, flags_byte: u8, granule: i64, seq: u32, packets: &[&[u8]]) -> Vec<u8> {
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

fn open(bytes: Vec<u8>) -> Box<dyn Demuxer> {
    let input: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    oxideav_ogg::demux::open(input, &NullCodecResolver).unwrap()
}

fn packets(d: &mut dyn Demuxer) -> Vec<Packet> {
    let mut out = Vec::new();
    loop {
        match d.next_packet() {
            Ok(p) => out.push(p),
            Err(Error::Eof) => return out,
            Err(e) => panic!("demux: {e}"),
        }
    }
}

/// A Kate header packet: type, `kate\0\0\0`, a reserved byte, `body`.
fn kate_header(kind: u8, body: &[u8]) -> Vec<u8> {
    [&[kind][..], b"kate\0\0\0", &[0], body].concat()
}

/// Kate ID header (64 bytes): bitstream 0.7, `headers` header packets,
/// UTF-8, granule shift 32, granule rate 1000/1, "en", "subtitles".
fn kate_id(headers: u8) -> Vec<u8> {
    let mut body = vec![0, 7, headers, 0, 0, 0, 32];
    body.extend_from_slice(&[0; 8]);
    body.extend_from_slice(&1000u32.to_le_bytes());
    body.extend_from_slice(&1u32.to_le_bytes());
    let mut language = [0u8; 16];
    language[..2].copy_from_slice(b"en");
    let mut category = [0u8; 16];
    category[..9].copy_from_slice(b"subtitles");
    body.extend_from_slice(&language);
    body.extend_from_slice(&category);
    kate_header(0x80, &body)
}

/// A Vorbis comment block: vendor, then `KEY=value` pairs.
fn comment_block(vendor: &str, pairs: &[&str]) -> Vec<u8> {
    let mut out = (vendor.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(vendor.as_bytes());
    out.extend_from_slice(&(pairs.len() as u32).to_le_bytes());
    for pair in pairs {
        out.extend_from_slice(&(pair.len() as u32).to_le_bytes());
        out.extend_from_slice(pair.as_bytes());
    }
    out
}

/// A Kate text event: start, duration and backlink in granule units, then
/// the text.
fn kate_event(start: u64, duration: u64, text: &str) -> Vec<u8> {
    let mut out = vec![0x00];
    out.extend_from_slice(&start.to_le_bytes());
    out.extend_from_slice(&duration.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
    out.extend_from_slice(text.as_bytes());
    out
}

#[test]
fn kate_is_a_subtitle_stream_timed_by_its_granules() {
    const SERIAL: u32 = 0x4B41;
    let id = kate_id(3);
    let comments = kate_header(0x81, &comment_block("fixture", &["ENCODER=kateenc"]));
    let regions = kate_header(0x82, &[0, 0]);
    let first = kate_event(500, 1500, "Hello");
    let second = kate_event(2500, 2500, "World");
    let bytes = [
        page(SERIAL, flags::FIRST_PAGE, 0, 0, &[&id]),
        page(SERIAL, 0, 0, 1, &[&comments, &regions]),
        page(SERIAL, 0, 500 << 32, 2, &[&first]),
        // Base 500 (the first event is still on screen), offset 2000.
        page(SERIAL, 0, (500 << 32) | 2000, 3, &[&second]),
        page(SERIAL, flags::LAST_PAGE, 5000 << 32, 4, &[&[0x7F]]),
    ]
    .concat();
    let mut d = open(bytes);
    let stream = d.streams()[0].clone();
    assert_eq!(stream.params.media_type, MediaType::Subtitle);
    assert_eq!(stream.params.codec_id.as_str(), "kate");
    assert_eq!(stream.time_base, TimeBase::new(1, 1000));
    assert_eq!(stream.params.extradata, [id.clone(), comments, regions].concat(), "every header, ID first");
    assert!(d.metadata().contains(&("encoder".into(), "kateenc".into())), "{:?}", d.metadata());
    let got: Vec<(Vec<u8>, Option<i64>, bool)> =
        packets(d.as_mut()).into_iter().map(|p| (p.data, p.pts, p.flags.keyframe)).collect();
    assert_eq!(got, [(first, Some(500), true), (second, Some(2500), true), (vec![0x7F], Some(5000), true)]);
}

#[test]
fn kate_header_count_comes_from_the_id_header() {
    // Nine headers, as libkate writes them (ID, comments, seven lists).
    const SERIAL: u32 = 9;
    let id = kate_id(9);
    let mut bytes = page(SERIAL, flags::FIRST_PAGE, 0, 0, &[&id]);
    let headers: Vec<Vec<u8>> = (0x81..=0x88).map(|kind| kate_header(kind, &[0, 0])).collect();
    let refs: Vec<&[u8]> = headers.iter().map(Vec::as_slice).collect();
    bytes.extend(page(SERIAL, 0, 0, 1, &refs));
    let event = kate_event(0, 1000, "x");
    bytes.extend(page(SERIAL, flags::LAST_PAGE, 0, 2, &[&event]));
    let mut d = open(bytes);
    let data: Vec<Vec<u8>> = packets(d.as_mut()).into_iter().map(|p| p.data).collect();
    assert_eq!(data, [event]);
}

/// The CMML ident header: version 2.1, granule rate 1000/1 (little-endian
/// 64-bit), granule shift 32.
fn cmml_ident() -> Vec<u8> {
    let mut out = b"CMML\0\0\0\0".to_vec();
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1000i64.to_le_bytes());
    out.extend_from_slice(&1i64.to_le_bytes());
    out.push(32);
    out
}

#[test]
fn cmml_is_a_subtitle_stream_timed_by_the_upper_granule_bits() {
    const SERIAL: u32 = 0x434D;
    let ident = cmml_ident();
    let preamble: &[u8] = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<?cmml lang=\"en\"?>";
    let head: &[u8] = b"<head>\n<title>Fixture</title>\n</head>";
    let one: &[u8] = b"<clip id=\"one\" track=\"main\"><desc>First</desc></clip>";
    let two: &[u8] = b"<clip id=\"two\" track=\"main\"><desc>Second</desc></clip>";
    let end: &[u8] = b"<clip track=\"main\"/>";
    let bytes = [
        page(SERIAL, flags::FIRST_PAGE, 0, 0, &[&ident]),
        page(SERIAL, 0, 0, 1, &[preamble, head]),
        page(SERIAL, 0, 1500 << 32, 2, &[one]),
        page(SERIAL, 0, (4000 << 32) | 1500, 3, &[two]),
        page(SERIAL, flags::LAST_PAGE, (6000 << 32) | 4000, 4, &[end]),
    ]
    .concat();
    let mut d = open(bytes);
    let stream = d.streams()[0].clone();
    assert_eq!(stream.params.media_type, MediaType::Subtitle);
    assert_eq!(stream.params.codec_id.as_str(), "cmml");
    assert_eq!(stream.time_base, TimeBase::new(1, 1000));
    assert!(stream.params.extradata.starts_with(&ident), "the ident header first");
    let got: Vec<(Vec<u8>, Option<i64>)> = packets(d.as_mut()).into_iter().map(|p| (p.data, p.pts)).collect();
    assert_eq!(got, [(one.to_vec(), Some(1500)), (two.to_vec(), Some(4000)), (end.to_vec(), Some(6000))]);
}

/// An OGM stream header (FFmpeg `ogm_header`, type 1): the flavour padded
/// to 8 bytes, the codec field, size, time unit, samples per unit, default
/// length, buffer size, bits per sample, then the video or audio tail.
fn ogm_header(flavour: &[u8], codec: &[u8; 4], time_unit: u64, spu: u64, tail: &[u8]) -> Vec<u8> {
    let mut out = vec![1];
    let mut name = [0u8; 8];
    name[..flavour.len()].copy_from_slice(flavour);
    out.extend_from_slice(&name);
    out.extend_from_slice(codec);
    out.extend_from_slice(&((52 + tail.len().saturating_sub(8)) as u32).to_le_bytes());
    out.extend_from_slice(&time_unit.to_le_bytes());
    out.extend_from_slice(&spu.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(tail);
    out
}

/// An OGM data packet: the flag byte (keyframe bit 3; `lb` in bits 1 and
/// 6-7), `lb` little-endian length bytes, the payload.
fn ogm_packet(key: bool, length: &[u8], payload: &[u8]) -> Vec<u8> {
    let lb = length.len() as u8;
    let flag = (if key { 8 } else { 0 }) | ((lb & 4) >> 1) | ((lb & 3) << 6);
    [&[flag][..], length, payload].concat()
}

#[test]
fn ogm_text_and_video_streams_follow_ffmpegs_oggparseogm() {
    const TEXT: u32 = 2;
    const VIDEO: u32 = 1;
    // bots01.ogm's text header: time unit 10000, one sample per unit (1 kHz).
    let text_id = ogm_header(b"text", &[0; 4], 10_000, 1, &[0; 12]);
    let text_comments = [&b"\x03vorbis"[..], &comment_block("fixture", &["LANGUAGE=English"]), &[1]].concat();
    // 640x480 XVID, time unit 333667 (29.97 fps).
    let video_id = ogm_header(b"video", b"XVID", 333_667, 1, &[0x80, 2, 0, 0, 0xE0, 1, 0, 0]);
    let blank = ogm_packet(true, &[0x78, 0x08], b" \0");
    let line = ogm_packet(true, &[0xFF, 0x05], b"Imperial Calendar Year 955\r\n\0");
    let frame = ogm_packet(true, &[], b"VOP");
    let delta = ogm_packet(false, &[1, 0, 0], b"P");
    let bytes = [
        page(VIDEO, flags::FIRST_PAGE, 0, 0, &[&video_id]),
        page(TEXT, flags::FIRST_PAGE, 0, 0, &[&text_id]),
        page(VIDEO, 0, 0, 1, &[b"\x03video"]),
        // No comment header on the text stream would leave its first data
        // packet a header; this one has one.
        page(TEXT, 0, 0, 1, &[&text_comments]),
        page(TEXT, 0, 0, 2, &[&blank]),
        // Two packets: the granule (5) is the start of the last one.
        page(VIDEO, 0, 5, 2, &[&frame, &delta]),
        page(TEXT, flags::LAST_PAGE, 2168, 3, &[&line]),
        page(VIDEO, flags::LAST_PAGE, 6, 3, &[&frame]),
    ]
    .concat();
    let mut d = open(bytes);
    let (video, text) = (d.streams()[0].clone(), d.streams()[1].clone());
    assert_eq!(video.params.media_type, MediaType::Video);
    assert_eq!(video.params.tag, Some(CodecTag::fourcc(b"XVID")));
    assert_eq!((video.params.width, video.params.height), (Some(640), Some(480)));
    assert_eq!(video.time_base, TimeBase::new(333_667, 10_000_000));
    assert!(video.params.extradata.is_empty(), "FFmpeg gives OGM video no extradata");
    assert_eq!(text.params.media_type, MediaType::Subtitle);
    assert_eq!(text.params.codec_id.as_str(), "text");
    assert_eq!(text.time_base, TimeBase::new(1, 1000));
    assert!(d.metadata().contains(&("language".into(), "English".into())), "{:?}", d.metadata());
    type Seen = (u32, Vec<u8>, Option<i64>, Option<i64>, bool);
    let got: Vec<Seen> = packets(d.as_mut())
        .into_iter()
        .map(|p| (p.stream_index, p.data, p.pts, p.duration, p.flags.keyframe))
        .collect();
    assert_eq!(
        got,
        [
            (1, b" \0".to_vec(), Some(0), Some(2168), true),
            (0, b"VOP".to_vec(), None, Some(0), true),
            (0, b"P".to_vec(), Some(5), Some(1), false),
            (1, b"Imperial Calendar Year 955\r\n\0".to_vec(), Some(2168), Some(1535), true),
            (0, b"VOP".to_vec(), Some(6), Some(0), true),
        ]
    );
}

#[test]
fn ogm_headers_end_at_the_first_packet_without_the_header_bit() {
    // No comment header: the first data packet must not be swallowed.
    const SERIAL: u32 = 3;
    let text_id = ogm_header(b"text", &[0; 4], 10_000, 1, &[0; 12]);
    let cue = ogm_packet(true, &[0xE8, 0x03], b"Hi\0");
    let bytes = [page(SERIAL, flags::FIRST_PAGE, 0, 0, &[&text_id]), page(SERIAL, flags::LAST_PAGE, 0, 1, &[&cue])].concat();
    let mut d = open(bytes);
    let got: Vec<(Vec<u8>, Option<i64>)> = packets(d.as_mut()).into_iter().map(|p| (p.data, p.duration)).collect();
    assert_eq!(got, [(b"Hi\0".to_vec(), Some(1000))]);
}

#[test]
fn ogm_rejects_zero_timing_and_drops_packets_shorter_than_their_length_field() {
    const SERIAL: u32 = 4;
    // time_unit 0: FFmpeg's "Invalid timing values." leaves the stream unusable.
    let broken = ogm_header(b"text", &[0; 4], 0, 1, &[0; 12]);
    let mut d = open(page(SERIAL, flags::FIRST_PAGE | flags::LAST_PAGE, 0, 0, &[&broken]));
    assert_eq!(d.streams()[0].params.media_type, MediaType::Unknown);
    assert!(packets(d.as_mut()).is_empty());

    let text_id = ogm_header(b"text", &[0; 4], 10_000, 1, &[0; 12]);
    // Flag 0xC0 declares three length bytes; the packet holds two.
    let short: &[u8] = &[0xC0, 1, 2];
    let fine = ogm_packet(true, &[5], b"ok\0");
    let bytes = [
        page(SERIAL, flags::FIRST_PAGE, 0, 0, &[&text_id]),
        page(SERIAL, flags::LAST_PAGE, 0, 1, &[short, &fine]),
    ]
    .concat();
    let mut d = open(bytes);
    let got: Vec<Vec<u8>> = packets(d.as_mut()).into_iter().map(|p| p.data).collect();
    assert_eq!(got, [b"ok\0".to_vec()]);
}

/// An OGM audio stream (WAVE tag `tag` in hex, 48 kHz) whose frames the
/// muxer cut into `cuts` arbitrary chunks, as OggDS muxers do: the first
/// two chunks on one page (granule 1000), the rest on the last (granule
/// 3000).
fn ogm_audio(tag: &[u8; 4], stream: &[u8], cuts: &[usize]) -> Vec<u8> {
    const SERIAL: u32 = 5;
    let id = ogm_header(b"audio", tag, 10_000_000, 48_000, &[2, 0, 1, 0, 0xC0, 0x5D, 0, 0]);
    let mut chunks = Vec::new();
    let mut at = 0;
    for &end in cuts.iter().chain([stream.len()].iter()) {
        chunks.push(ogm_packet(true, &[0, 6], &stream[at..end]));
        at = end;
    }
    let refs: Vec<&[u8]> = chunks.iter().map(Vec::as_slice).collect();
    [
        page(SERIAL, flags::FIRST_PAGE, 0, 0, &[&id]),
        page(SERIAL, 0, 0, 1, &[b"\x03audio\0\0"]),
        page(SERIAL, 0, 1000, 2, &refs[..2]),
        page(SERIAL, flags::LAST_PAGE, 3000, 3, &refs[2..]),
    ]
    .concat()
}

/// `n` AC-3 frames of 128 bytes (32 kbps at 48 kHz: frmsizecod 0, bsid 8),
/// frame `i` filled with `i`.
fn ac3_frames(n: u8) -> Vec<Vec<u8>> {
    (0..n).map(|i| [&[0x0B, 0x77, 0, 0, 0x00, 0x40][..], &[i; 122]].concat()).collect()
}

#[test]
fn ogm_ac3_chunks_play_as_whole_frames_like_ffmpegs_parser() {
    // FFmpeg's ogm_header sets AVSTREAM_PARSE_FULL for non-AAC audio: the
    // AC-3 parser rebuilds frames from the chunks. A chunk's pts (granule_is_start:
    // only the packet ending a page has one) goes to the first frame starting
    // in it; each frame lasts its 1536 samples.
    let frames = ac3_frames(5);
    let stream = frames.concat();
    // Chunks [0, 200) and [200, 300) on the first page, [300, 640 - 30) and
    // a cut-off last frame on the second.
    let bytes = ogm_audio(b"2000", &stream[..640 - 30], &[200, 300]);
    let mut d = open(bytes);
    assert_eq!(d.streams()[0].params.media_type, MediaType::Audio);
    assert_eq!(d.streams()[0].params.tag, Some(CodecTag::wave_format(0x2000)));
    let got: Vec<(Vec<u8>, Option<i64>, Option<i64>)> =
        packets(d.as_mut()).into_iter().map(|p| (p.data, p.pts, p.duration)).collect();
    let expected: Vec<(Vec<u8>, Option<i64>, Option<i64>)> = vec![
        (frames[0].clone(), None, Some(1536)),
        (frames[1].clone(), None, Some(1536)),
        // Starts at byte 256, inside the page's last chunk [200, 300).
        (frames[2].clone(), Some(1000), Some(1536)),
        // Starts at byte 384, inside the last chunk [300, 610).
        (frames[3].clone(), Some(3000), Some(1536)),
        // At the end the parser flushes the cut-off frame, as FFmpeg's does
        // (its decoder conceals it).
        (frames[4][..128 - 30].to_vec(), None, Some(1536)),
    ];
    assert_eq!(got, expected);
}

#[test]
fn ogm_mpeg_audio_chunks_play_as_whole_frames() {
    // MPEG-1 Layer II, 64 kbps, 48 kHz: 192-byte frames of 1152 samples,
    // after three bytes of junk the parser skips.
    let frames: Vec<Vec<u8>> = (0..3u8).map(|i| [&[0xFF, 0xFD, 0x44, 0xC0][..], &[i; 188]].concat()).collect();
    let stream = [&[0x00, 0x11, 0x22][..], &frames.concat()].concat();
    let mut d = open(ogm_audio(b"0050", &stream, &[100, 250]));
    let got: Vec<(Vec<u8>, Option<i64>)> = packets(d.as_mut()).into_iter().map(|p| (p.data, p.duration)).collect();
    let expected: Vec<(Vec<u8>, Option<i64>)> = frames.into_iter().map(|f| (f, Some(1152))).collect();
    assert_eq!(got, expected);
}

#[test]
fn ogm_pcm_chunks_pass_through() {
    // PCM has no parser: chunks stay chunks.
    let stream: Vec<u8> = (0..=255).collect();
    let mut d = open(ogm_audio(b"0001", &stream, &[100, 200]));
    let got: Vec<Vec<u8>> = packets(d.as_mut()).into_iter().map(|p| p.data).collect();
    assert_eq!(got, [stream[..100].to_vec(), stream[100..200].to_vec(), stream[200..].to_vec()]);
}
