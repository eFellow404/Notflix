//! Pure-Rust Matroska / WebM (EBML) chapter parser.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use super::{read_exact_count, read_up_to, ChapterMarker};

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
const MAX_TOP_LEVEL_ELEMENTS: usize = 512;

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

/// Validate the EBML header magic and seek past the EBML header element.
fn skip_ebml_header<R: Read + Seek>(fp: &mut R, file_size: u64) -> std::io::Result<bool> {
    let mut magic = [0u8; 4];
    if read_exact_count(fp, &mut magic)? < 4 || magic != EBML_HEADER_MAGIC {
        return Ok(false);
    }
    fp.seek(SeekFrom::Start(0))?;

    let _ = read_ebml_vint_file(fp, true);
    let (ebml_size_opt, _) = read_ebml_vint_file(fp, false);
    let Some(ebml_size) = ebml_size_opt else {
        return Ok(false);
    };
    let Some(after_header) = fp.stream_position()?.checked_add(ebml_size) else {
        return Ok(false);
    };
    if after_header > file_size {
        return Ok(false);
    }
    fp.seek(SeekFrom::Start(after_header))?;
    Ok(true)
}

/// Read and parse a `Chapters` element payload when its size is known and bounded.
fn read_chapters_element<R: Read>(
    fp: &mut R,
    size_opt: Option<u64>,
) -> std::io::Result<Option<Vec<ChapterMarker>>> {
    if let Some(sz) = size_opt {
        if sz <= MAX_CHAPTERS_BLOB_BYTES {
            let blob = read_up_to(fp, sz as usize)?;
            return Ok(Some(parse_mkv_chapters_blob(&blob)));
        }
    }
    Ok(None)
}

enum SeekHeadOutcome {
    Found(Vec<ChapterMarker>),
    Continue,
    Stop,
}

/// Read a `SeekHead` element and, if it references `Chapters`, jump directly there.
fn try_follow_seekhead<R: Read + Seek>(
    fp: &mut R,
    size: u64,
    elem_start: u64,
    seg_data_start: u64,
    file_size: u64,
    chapters_offset: &mut Option<u64>,
) -> std::io::Result<SeekHeadOutcome> {
    let blob = read_up_to(fp, size as usize)?;
    if let Some(found) = find_chapters_offset_in_seekhead(&blob, seg_data_start) {
        *chapters_offset = Some(found);
    }
    if let Some(off) = *chapters_offset {
        if off < file_size {
            fp.seek(SeekFrom::Start(off))?;
            let (cid, _) = read_ebml_vint_file(fp, true);
            let (csize, _) = read_ebml_vint_file(fp, false);
            if cid == Some(EBML_CHAPTERS_ID) {
                if let Some(chapters) = read_chapters_element(fp, csize)? {
                    return Ok(SeekHeadOutcome::Found(chapters));
                }
            }
            let Some(next_pos) = elem_start.checked_add(size) else {
                return Ok(SeekHeadOutcome::Stop);
            };
            fp.seek(SeekFrom::Start(next_pos))?;
        }
    }
    Ok(SeekHeadOutcome::Continue)
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

    if !skip_ebml_header(&mut fp, file_size)? {
        return Ok(Vec::new());
    }

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
    while fp.stream_position()? < seg_end && scanned < MAX_TOP_LEVEL_ELEMENTS {
        scanned += 1;
        let (eid_opt, _) = read_ebml_vint_file(&mut fp, true);
        let Some(eid) = eid_opt else {
            break;
        };
        let (size_opt, _) = read_ebml_vint_file(&mut fp, false);
        let elem_start = fp.stream_position()?;

        if eid == EBML_CHAPTERS_ID {
            if let Some(chapters) = read_chapters_element(&mut fp, size_opt)? {
                return Ok(chapters);
            }
        }

        if eid == EBML_SEEKHEAD_ID {
            if let Some(sz) = size_opt.filter(|&sz| sz <= MAX_SEEKHEAD_BLOB_BYTES) {
                match try_follow_seekhead(
                    &mut fp,
                    sz,
                    elem_start,
                    seg_data_start,
                    file_size,
                    &mut chapters_offset,
                )? {
                    SeekHeadOutcome::Found(chapters) => return Ok(chapters),
                    SeekHeadOutcome::Continue => continue,
                    SeekHeadOutcome::Stop => break,
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
