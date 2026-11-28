//! Pure-Rust MP4 / M4V / MOV Nero `chpl` chapter parser.

use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::Path;

use super::{read_exact_count, read_up_to, ChapterMarker};

const MAX_CHPL_BLOB_BYTES: u64 = 2 * 1024 * 1024;

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
