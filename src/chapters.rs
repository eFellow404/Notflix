//! Pure-Rust container chapter extraction (MKV/WebM EBML and MP4 `chpl`).

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::intro_detector::{IntroSpan, MAX_INTRO_SEC, MIN_INTRO_SEC};

static INTRO_CHAPTER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)",
        r"(?:^|[\s\-_:/(\[])",
        r"(?:intro(?:duction)?|opening(?:\s+(?:credits?|titles?|theme|sequence))?|",
        r"main\s+titles?|title\s+sequence|theme\s+song|op(?:\s*\d+)?)",
        r"(?:$|[\s\-_:/)\]])",
    ))
    .expect("INTRO_CHAPTER_RE must be a valid regular expression")
});

// Matroska / EBML element IDs
const EBML_HEADER_MAGIC: [u8; 4] = [0x1a, 0x45, 0xdf, 0xa3];
const EBML_SEGMENT_ID: u64 = 0x1853_8067;
const EBML_SEEKHEAD_ID: u64 = 0x114d_9b74;
const EBML_CHAPTERS_ID: u64 = 0x1043_a770;
const EBML_CHAPTERS_ID_BYTES: &[u8] = &[0x10, 0x43, 0xa7, 0x70];
const EBML_CLUSTER_ID: u64 = 0x1f43_b675;
const EBML_SEEK_ID: u64 = 0x4dbb;
const EBML_SEEK_TARGET_ID: u64 = 0x53ab;
const EBML_SEEK_POSITION_ID: u64 = 0x53ac;
const EBML_EDITION_ENTRY_ID: u64 = 0x45b9;
const EBML_CHAPTER_ATOM_ID: u64 = 0xb6;
const EBML_CHAPTER_TIME_START_ID: u64 = 0x91;
const EBML_CHAPTER_TIME_END_ID: u64 = 0x92;
const EBML_CHAPTER_DISPLAY_ID: u64 = 0x80;
const EBML_CHAPTER_STRING_ID: u64 = 0x85;

const MAX_CHAPTERS_BLOB_BYTES: u64 = 4 * 1024 * 1024;
const MAX_SEEKHEAD_BLOB_BYTES: u64 = 1024 * 1024;
const MAX_CHPL_BLOB_BYTES: u64 = 2 * 1024 * 1024;

/// A chapter entry extracted from an MKV/WebM or MP4 container.
#[derive(Debug, Clone, PartialEq)]
pub struct ChapterMarker {
    pub start_sec: f64,
    pub end_sec: Option<f64>,
    pub title: String,
}

/// Return `true` if a chapter title explicitly labels an intro/opening sequence.
pub fn is_intro_chapter_title(title: &str) -> bool {
    !title.is_empty() && INTRO_CHAPTER_RE.is_match(title)
}

/// Determine EBML VINT byte length from its leading byte (`0` if invalid).
#[inline]
fn vint_length(first: u8) -> usize {
    if first == 0 {
        0
    } else {
        (first.leading_zeros() as usize) + 1
    }
}

/// Decode an EBML VINT from its leading byte and remaining `length - 1` bytes.
///
/// Returns `None` for the all-ones "unknown size" sentinel when `!keep_marker`.
#[inline]
fn decode_ebml_vint(first: u8, rest: &[u8], keep_marker: bool) -> Option<u64> {
    let length = rest.len() + 1;
    let mask = 0x80u8 >> (length - 1);
    let mut val = if keep_marker {
        u64::from(first)
    } else {
        u64::from(first & (mask - 1))
    };
    for &byte in rest {
        val = (val << 8) | u64::from(byte);
    }
    if !keep_marker && val == (1u64 << (7 * length)) - 1 {
        return None;
    }
    Some(val)
}

/// Read an EBML variable-length integer from `data` at `pos`.
fn read_ebml_vint(data: &[u8], pos: usize, keep_marker: bool) -> (Option<u64>, usize) {
    let Some(&first) = data.get(pos) else {
        return (None, pos);
    };
    let length = vint_length(first);
    if length == 0 {
        return (None, pos);
    }
    let Some(end) = pos.checked_add(length) else {
        return (None, pos);
    };
    if end > data.len() {
        return (None, pos);
    }

    (
        decode_ebml_vint(first, &data[pos + 1..end], keep_marker),
        end,
    )
}

/// Read an EBML VINT directly from a binary file stream.
fn read_ebml_vint_file<R: Read>(fp: &mut R, keep_marker: bool) -> (Option<u64>, usize) {
    let mut first_buf = [0u8; 1];
    match read_exact_count(fp, &mut first_buf) {
        Ok(1) => {}
        _ => return (None, 0),
    }

    let first = first_buf[0];
    let length = vint_length(first);
    if length == 0 {
        return (None, 1);
    }

    let mut rest = [0u8; 7];
    let need = length - 1;
    let got = match read_exact_count(fp, &mut rest[..need]) {
        Ok(n) => n,
        Err(_) => return (None, 1),
    };
    if got < need {
        return (None, 1 + got);
    }

    (decode_ebml_vint(first, &rest[..need], keep_marker), length)
}

/// Read until `buf` is full or EOF is reached, returning total bytes read.
fn read_exact_count<R: Read>(fp: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match fp.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Read up to `limit` bytes from `fp` without preallocating unbounded memory.
fn read_up_to<R: Read>(fp: &mut R, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(limit.min(64 * 1024));
    fp.take(limit as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Decode a big-endian unsigned integer slice into `u64`, returning `None` on empty or overflow.
fn be_bytes_to_u64(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() {
        return None;
    }
    let mut val: u64 = 0;
    for &b in bytes {
        val = val.checked_mul(256)?.checked_add(u64::from(b))?;
    }
    Some(val)
}

struct EbmlElementIter<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> EbmlElementIter<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
}

impl<'a> Iterator for EbmlElementIter<'a> {
    type Item = (u64, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let (eid_opt, next_pos) = read_ebml_vint(self.buf, self.pos, true);
        let eid = eid_opt?;
        let (size_opt, data_pos) = read_ebml_vint(self.buf, next_pos, false);
        let size = usize::try_from(size_opt?).ok()?;
        let end = data_pos.checked_add(size)?;
        if end > self.buf.len() {
            return None;
        }
        self.pos = end;
        Some((eid, &self.buf[data_pos..end]))
    }
}

fn parse_mkv_chapters_blob(blob: &[u8]) -> Vec<ChapterMarker> {
    let mut chapters = Vec::new();
    // Chapters (0x1043A770) -> EditionEntry (0x45B9) -> ChapterAtom (0xB6)
    for (eid, payload) in EbmlElementIter::new(blob) {
        if eid != EBML_EDITION_ENTRY_ID {
            continue;
        }
        for (sub_id, sub_payload) in EbmlElementIter::new(payload) {
            if sub_id != EBML_CHAPTER_ATOM_ID {
                continue;
            }
            let mut start_ns: Option<u64> = None;
            let mut end_ns: Option<u64> = None;
            let mut title = String::new();
            for (atom_id, atom_val) in EbmlElementIter::new(sub_payload) {
                if atom_id == EBML_CHAPTER_TIME_START_ID && !atom_val.is_empty() {
                    start_ns = be_bytes_to_u64(atom_val);
                } else if atom_id == EBML_CHAPTER_TIME_END_ID && !atom_val.is_empty() {
                    end_ns = be_bytes_to_u64(atom_val);
                } else if atom_id == EBML_CHAPTER_DISPLAY_ID {
                    for (disp_id, disp_val) in EbmlElementIter::new(atom_val) {
                        if disp_id == EBML_CHAPTER_STRING_ID && title.is_empty() {
                            title = String::from_utf8_lossy(disp_val).trim().to_string();
                        }
                    }
                }
            }
            if let Some(start) = start_ns {
                chapters.push(ChapterMarker {
                    start_sec: (start as f64) / 1_000_000_000.0,
                    end_sec: end_ns.map(|end| (end as f64) / 1_000_000_000.0),
                    title,
                });
            }
        }
    }
    chapters.sort_by(|a, b| a.start_sec.total_cmp(&b.start_sec));
    chapters
}

fn find_chapters_offset_in_seekhead(seek_blob: &[u8], seg_data_start: u64) -> Option<u64> {
    let mut chapters_offset: Option<u64> = None;
    for (sid, spayload) in EbmlElementIter::new(seek_blob) {
        if sid != EBML_SEEK_ID {
            continue;
        }
        let mut target_id: Option<&[u8]> = None;
        let mut target_pos: Option<u64> = None;
        for (fid, fval) in EbmlElementIter::new(spayload) {
            if fid == EBML_SEEK_TARGET_ID {
                target_id = Some(fval);
            } else if fid == EBML_SEEK_POSITION_ID && !fval.is_empty() {
                target_pos = be_bytes_to_u64(fval);
            }
        }
        if target_id == Some(EBML_CHAPTERS_ID_BYTES) {
            if let Some(pos) = target_pos {
                if let Some(abs_offset) = seg_data_start.checked_add(pos) {
                    chapters_offset = Some(abs_offset);
                }
            }
        }
    }
    chapters_offset
}

/// Extract chapter markers from an MKV/WebM file without external tools.
///
/// Skips media `Cluster` elements via seeking and follows `SeekHead` pointers
/// when present so scanning multi-GB files takes milliseconds.
pub fn extract_mkv_chapters(file_path: &Path) -> Vec<ChapterMarker> {
    extract_mkv_chapters_inner(file_path).unwrap_or_default()
}

fn extract_mkv_chapters_inner(file_path: &Path) -> std::io::Result<Vec<ChapterMarker>> {
    let file = File::open(file_path)?;
    let file_size = file.metadata()?.len();
    let mut fp = BufReader::new(file);

    let mut magic = [0u8; 4];
    if read_exact_count(&mut fp, &mut magic)? < 4 || magic != EBML_HEADER_MAGIC {
        return Ok(Vec::new());
    }
    fp.seek(SeekFrom::Start(0))?;

    // Read EBML header and skip past it
    let _ = read_ebml_vint_file(&mut fp, true);
    let (ebml_size_opt, _) = read_ebml_vint_file(&mut fp, false);
    let Some(ebml_size) = ebml_size_opt else {
        return Ok(Vec::new());
    };
    let Some(after_header) = fp.stream_position()?.checked_add(ebml_size) else {
        return Ok(Vec::new());
    };
    if after_header > file_size {
        return Ok(Vec::new());
    }
    fp.seek(SeekFrom::Start(after_header))?;

    // Segment element (0x18538067)
    let (seg_id, _) = read_ebml_vint_file(&mut fp, true);
    if seg_id != Some(EBML_SEGMENT_ID) {
        return Ok(Vec::new());
    }
    let (seg_size, _) = read_ebml_vint_file(&mut fp, false);
    let seg_data_start = fp.stream_position()?;
    let seg_end = match seg_size {
        Some(sz) => seg_data_start
            .checked_add(sz)
            .map_or(file_size, |e| e.min(file_size)),
        None => file_size,
    };

    let mut chapters_offset: Option<u64> = None;
    let mut scanned = 0usize;
    while fp.stream_position()? < seg_end && scanned < 512 {
        scanned += 1;
        let (eid_opt, _) = read_ebml_vint_file(&mut fp, true);
        let Some(eid) = eid_opt else {
            break;
        };
        let (size_opt, _) = read_ebml_vint_file(&mut fp, false);
        let elem_start = fp.stream_position()?;

        if eid == EBML_CHAPTERS_ID {
            if let Some(sz) = size_opt {
                if sz <= MAX_CHAPTERS_BLOB_BYTES {
                    let blob = read_up_to(&mut fp, sz as usize)?;
                    return Ok(parse_mkv_chapters_blob(&blob));
                }
            }
        }

        if eid == EBML_SEEKHEAD_ID {
            if let Some(sz) = size_opt {
                if sz <= MAX_SEEKHEAD_BLOB_BYTES {
                    let blob = read_up_to(&mut fp, sz as usize)?;
                    if let Some(found) = find_chapters_offset_in_seekhead(&blob, seg_data_start) {
                        chapters_offset = Some(found);
                    }
                    if let Some(off) = chapters_offset {
                        if off < file_size {
                            fp.seek(SeekFrom::Start(off))?;
                            let (cid, _) = read_ebml_vint_file(&mut fp, true);
                            let (csize, _) = read_ebml_vint_file(&mut fp, false);
                            if cid == Some(EBML_CHAPTERS_ID) {
                                if let Some(csz) = csize {
                                    if csz <= MAX_CHAPTERS_BLOB_BYTES {
                                        let cblob = read_up_to(&mut fp, csz as usize)?;
                                        return Ok(parse_mkv_chapters_blob(&cblob));
                                    }
                                }
                            }
                            let Some(next_pos) = elem_start.checked_add(sz) else {
                                break;
                            };
                            fp.seek(SeekFrom::Start(next_pos))?;
                        }
                    }
                    continue;
                }
            }
        }

        let Some(sz) = size_opt else {
            break;
        };
        // Stop once we hit media clusters unless SeekHead already pointed us
        if eid == EBML_CLUSTER_ID && chapters_offset.is_none() {
            break;
        }
        let Some(next_pos) = elem_start.checked_add(sz) else {
            break;
        };
        fp.seek(SeekFrom::Start(next_pos))?;
    }

    Ok(Vec::new())
}

/// Parse an MP4 Nero `chpl` atom body.
fn parse_mp4_chpl(payload: &[u8]) -> Vec<ChapterMarker> {
    if payload.len() < 9 {
        return Vec::new();
    }
    let mut offset: usize = if payload[0] == 1 { 8 } else { 4 };
    if offset >= payload.len() {
        return Vec::new();
    }
    let count = usize::from(payload[offset]);
    offset += 1;

    let mut chapters = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        if !offset
            .checked_add(9)
            .is_some_and(|end| end <= payload.len())
        {
            break;
        }
        let ts_bytes: [u8; 8] = match payload[offset..offset + 8].try_into() {
            Ok(b) => b,
            Err(_) => break,
        };
        let timestamp_100ns = u64::from_be_bytes(ts_bytes);
        offset += 8;

        let title_len = usize::from(payload[offset]);
        offset += 1;
        let Some(title_end) = offset.checked_add(title_len) else {
            break;
        };
        if title_end > payload.len() {
            break;
        }
        let title = String::from_utf8_lossy(&payload[offset..title_end])
            .trim()
            .to_string();
        offset = title_end;

        chapters.push(ChapterMarker {
            start_sec: (timestamp_100ns as f64) / 10_000_000.0,
            end_sec: None,
            title,
        });
    }
    chapters.sort_by(|a, b| a.start_sec.total_cmp(&b.start_sec));
    chapters
}

/// Locate an MP4 box within `[start, end)` and return its `(body_start, box_end)`.
fn find_mp4_box(fp: &mut File, start: u64, end: u64, target: &[u8; 4]) -> Option<(u64, u64)> {
    let mut pos = start;
    while pos.checked_add(8).is_some_and(|p| p <= end) {
        fp.seek(SeekFrom::Start(pos)).ok()?;
        let mut hdr = [0u8; 8];
        if read_exact_count(fp, &mut hdr).ok()? < 8 {
            return None;
        }
        let raw_size = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
        let box_type = &hdr[4..8];

        let (size, hdr_len) = if raw_size == 1 {
            let mut ext = [0u8; 8];
            if read_exact_count(fp, &mut ext).ok()? < 8 {
                return None;
            }
            (u64::from_be_bytes(ext), 16u64)
        } else if raw_size == 0 {
            (end - pos, 8u64)
        } else {
            (u64::from(raw_size), 8u64)
        };

        let box_end = pos.checked_add(size)?;
        if size < hdr_len || box_end > end {
            return None;
        }
        if box_type == target {
            return Some((pos + hdr_len, box_end));
        }
        pos = box_end;
    }
    None
}

/// Extract Nero `chpl` chapter markers from an MP4/M4V/MOV container.
pub fn extract_mp4_chapters(file_path: &Path) -> Vec<ChapterMarker> {
    extract_mp4_chapters_inner(file_path).unwrap_or_default()
}

fn extract_mp4_chapters_inner(file_path: &Path) -> std::io::Result<Vec<ChapterMarker>> {
    let mut fp = File::open(file_path)?;
    let file_size = fp.metadata()?.len();

    let Some(moov) = find_mp4_box(&mut fp, 0, file_size, b"moov") else {
        return Ok(Vec::new());
    };
    let Some(udta) = find_mp4_box(&mut fp, moov.0, moov.1, b"udta") else {
        return Ok(Vec::new());
    };
    let Some(chpl) = find_mp4_box(&mut fp, udta.0, udta.1, b"chpl") else {
        return Ok(Vec::new());
    };

    let length = chpl.1.saturating_sub(chpl.0);
    if length == 0 || length > MAX_CHPL_BLOB_BYTES {
        return Ok(Vec::new());
    }
    fp.seek(SeekFrom::Start(chpl.0))?;
    let payload = read_up_to(&mut fp, length as usize)?;
    Ok(parse_mp4_chpl(&payload))
}

/// Extract container chapters for supported video formats (MKV, WebM, MP4, M4V, MOV).
pub fn extract_chapters(file_path: &Path) -> Vec<ChapterMarker> {
    let Some(ext) = file_path.extension().and_then(|e| e.to_str()) else {
        return Vec::new();
    };
    if ext.eq_ignore_ascii_case("mkv") || ext.eq_ignore_ascii_case("webm") {
        extract_mkv_chapters(file_path)
    } else if ext.eq_ignore_ascii_case("mp4")
        || ext.eq_ignore_ascii_case("m4v")
        || ext.eq_ignore_ascii_case("mov")
    {
        extract_mp4_chapters(file_path)
    } else {
        Vec::new()
    }
}

/// Return an `IntroSpan` if chapter markers explicitly label an opening/intro.
pub fn detect_intro_from_chapters(
    chapters: &[ChapterMarker],
    duration_sec: f64,
) -> Option<IntroSpan> {
    if chapters.is_empty() {
        return None;
    }

    for (idx, ch) in chapters.iter().enumerate() {
        if !is_intro_chapter_title(&ch.title) {
            continue;
        }
        let start = ch.start_sec.max(0.0);
        let end = ch
            .end_sec
            .or_else(|| chapters.get(idx + 1).map(|next| next.start_sec));
        let Some(end) = end else {
            continue;
        };
        let length = end - start;
        if !(MIN_INTRO_SEC..=MAX_INTRO_SEC).contains(&length) {
            continue;
        }
        if start > 600.0 {
            continue;
        }
        if duration_sec > 0.0 && start > duration_sec * 0.45 {
            continue;
        }
        return Some(IntroSpan {
            start_sec: start,
            end_sec: end,
            confidence: 0.95,
            source: "chapters".to_string(),
        });
    }
    None
}
