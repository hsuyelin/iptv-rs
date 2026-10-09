use crate::ts::{at, packet::Packet};

/// A reassembled PES with its header fields parsed.
#[derive(Debug, Clone)]
pub(crate) struct Pes {
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    /// Whole PES bytes, header included.
    pub data: Vec<u8>,
    /// Offset of the elementary-stream payload inside `data`.
    pub payload_start: usize,
}

impl Pes {
    pub fn payload(&self) -> &[u8] {
        self.data.get(self.payload_start..).unwrap_or(&[])
    }
}

/// Reassembles every PES carried on `pid`.
pub(crate) fn collect_pes(input: &[u8], packets: &[Packet], pid: u16) -> Vec<Pes> {
    let mut result = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    for packet in packets.iter().filter(|packet| packet.pid == pid) {
        if !packet.has_payload() {
            continue;
        }
        if packet.pusi && !current.is_empty() {
            let capacity = current.len();
            if let Some(pes) = parse_pes(std::mem::take(&mut current)) {
                result.push(pes);
            }
            current.reserve(capacity);
        }
        current.extend_from_slice(packet.payload(input));
    }
    if !current.is_empty() {
        if let Some(pes) = parse_pes(current) {
            result.push(pes);
        }
    }
    result
}

/// Parses the PES header; returns `None` for data that is not a PES.
pub(crate) fn parse_pes(data: Vec<u8>) -> Option<Pes> {
    if data.len() < 9 || at(&data, 0) != 0 || at(&data, 1) != 0 || at(&data, 2) != 1 {
        return None;
    }
    let flags = at(&data, 7);
    let payload_start = 9 + usize::from(at(&data, 8));
    if payload_start > data.len() {
        return None;
    }
    let pts = (flags & 0x80 != 0 && data.len() >= 14)
        .then(|| data.get(9..14).map(parse_timestamp))
        .flatten();
    let dts = (flags & 0x40 != 0 && data.len() >= 19)
        .then(|| data.get(14..19).map(parse_timestamp))
        .flatten();
    Some(Pes {
        pts,
        dts,
        data,
        payload_start,
    })
}

pub(crate) fn parse_timestamp(bytes: &[u8]) -> i64 {
    if bytes.len() < 5 {
        return 0;
    }
    (i64::from((at(bytes, 0) >> 1) & 0x07) << 30)
        | (i64::from(at(bytes, 1)) << 22)
        | (i64::from((at(bytes, 2) >> 1) & 0x7f) << 15)
        | (i64::from(at(bytes, 3)) << 7)
        | i64::from((at(bytes, 4) >> 1) & 0x7f)
}

pub(crate) fn encode_timestamp(prefix: u8, timestamp: i64) -> [u8; 5] {
    let value = u64::try_from(timestamp.max(0)).unwrap_or(0) & ((1u64 << 33) - 1);
    [
        (prefix << 4) | ((((value >> 30) & 0x07) as u8) << 1) | 1,
        ((value >> 22) & 0xff) as u8,
        ((((value >> 15) & 0x7f) as u8) << 1) | 1,
        ((value >> 7) & 0xff) as u8,
        (((value & 0x7f) as u8) << 1) | 1,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_round_trips() {
        for value in [0i64, 1, 90_000, 8_589_934_591, (1 << 33) - 1] {
            assert_eq!(parse_timestamp(&encode_timestamp(0x02, value)), value);
        }
    }

    #[test]
    fn rejects_non_pes() {
        assert!(parse_pes(vec![1, 2, 3]).is_none());
        assert!(parse_pes(vec![0, 0, 2, 0xe0, 0, 0, 0x80, 0, 0]).is_none());
        // Header length points past the end.
        assert!(parse_pes(vec![0, 0, 1, 0xe0, 0, 0, 0x80, 0, 9]).is_none());
    }
}
