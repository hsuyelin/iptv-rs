use crate::ts::at;

/// One elementary stream announced by the PMT.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StreamInfo {
    pub pid: u16,
    pub stream_type: u8,
}

pub(crate) const STREAM_H264: u8 = 0x1b;
pub(crate) const STREAM_AAC: u8 = 0x0f;

fn be12(hi: u8, lo: u8) -> usize {
    (usize::from(hi & 0x0f) << 8) | usize::from(lo)
}

fn pid13(hi: u8, lo: u8) -> u16 {
    (u16::from(hi & 0x1f) << 8) | u16::from(lo)
}

/// Returns the first non-zero program's PMT PID from a PAT payload.
pub(crate) fn parse_pat(payload: &[u8]) -> Option<u16> {
    if payload.len() < 8 {
        return None;
    }
    let mut offset = 1 + usize::from(at(payload, 0));
    if *payload.get(offset)? != 0x00 || offset + 8 > payload.len() {
        return None;
    }
    let section_length = be12(at(payload, offset + 1), at(payload, offset + 2));
    let end = offset + 3 + section_length.checked_sub(4)?;
    offset += 8;
    while offset + 4 <= end && offset + 4 <= payload.len() {
        let program =
            (u16::from(at(payload, offset)) << 8) | u16::from(at(payload, offset + 1));
        if program != 0 {
            return Some(pid13(at(payload, offset + 2), at(payload, offset + 3)));
        }
        offset += 4;
    }
    None
}

/// Lists the elementary streams in a PMT payload.
pub(crate) fn parse_pmt(payload: &[u8]) -> Vec<StreamInfo> {
    let Some(pointer) = payload.first().copied() else {
        return Vec::new();
    };
    let mut offset = 1 + usize::from(pointer);
    if payload.get(offset).copied() != Some(0x02) || offset + 12 > payload.len() {
        return Vec::new();
    }
    let section_length = be12(at(payload, offset + 1), at(payload, offset + 2));
    let program_info_length = be12(at(payload, offset + 10), at(payload, offset + 11));
    let Some(end) = offset.checked_add(3 + section_length.saturating_sub(4)) else {
        return Vec::new();
    };
    offset += 12 + program_info_length;
    let mut streams = Vec::new();
    while offset + 5 <= end && offset + 5 <= payload.len() {
        streams.push(StreamInfo {
            stream_type: at(payload, offset),
            pid: pid13(at(payload, offset + 1), at(payload, offset + 2)),
        });
        offset += 5 + be12(at(payload, offset + 3), at(payload, offset + 4));
    }
    streams
}
