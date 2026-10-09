use crate::{error::MediaError, ts::at};

pub(crate) const PACKET_LEN: usize = 188;
pub(crate) const SYNC_BYTE: u8 = 0x47;

/// Location of one transport packet inside the input.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Packet {
    pub pid: u16,
    pub pusi: bool,
    pub payload_start: usize,
    pub payload_end: usize,
}

impl Packet {
    pub fn has_payload(&self) -> bool {
        self.payload_start < self.payload_end
    }

    pub fn payload<'a>(&self, input: &'a [u8]) -> &'a [u8] {
        input
            .get(self.payload_start..self.payload_end)
            .unwrap_or(&[])
    }
}

/// Splits `input` into packet descriptors.
pub(crate) fn parse_packets(input: &[u8]) -> Result<Vec<Packet>, MediaError> {
    if input.is_empty() {
        return Err(MediaError::Empty);
    }
    if !input.len().is_multiple_of(PACKET_LEN) {
        return Err(MediaError::Misaligned { len: input.len() });
    }
    let mut packets = Vec::with_capacity(input.len() / PACKET_LEN);
    for (index, chunk) in input.as_chunks::<PACKET_LEN>().0.iter().enumerate() {
        let offset = index * PACKET_LEN;
        if at(chunk, 0) != SYNC_BYTE {
            return Err(MediaError::BadSync { offset });
        }
        let (b1, b2, b3) = (at(chunk, 1), at(chunk, 2), at(chunk, 3));
        let afc = (b3 >> 4) & 0x03;
        let mut payload_start = offset + 4;
        if afc == 2 || afc == 3 {
            payload_start += 1 + usize::from(at(chunk, 4));
        }
        let has_payload = afc == 1 || afc == 3;
        let end = offset + PACKET_LEN;
        if !has_payload || payload_start >= end {
            payload_start = end;
        }
        packets.push(Packet {
            pid: (u16::from(b1 & 0x1f) << 8) | u16::from(b2),
            pusi: b1 & 0x40 != 0,
            payload_start,
            payload_end: end,
        });
    }
    Ok(packets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_misaligned_and_bad_sync() {
        assert!(matches!(parse_packets(&[]), Err(MediaError::Empty)));
        let data = vec![0x47; PACKET_LEN * 2 + 17];
        assert!(matches!(
            parse_packets(&data),
            Err(MediaError::Misaligned { len }) if len == PACKET_LEN * 2 + 17
        ));
        let mut data = vec![0u8; PACKET_LEN * 2];
        data[0] = SYNC_BYTE;
        assert!(matches!(
            parse_packets(&data),
            Err(MediaError::BadSync { offset: 188 })
        ));
    }

    #[test]
    fn oversized_adaptation_field_yields_empty_payload() {
        let mut data = vec![0u8; PACKET_LEN];
        data[0] = SYNC_BYTE;
        data[3] = 0x30;
        data[4] = 250;
        let packets = parse_packets(&data).unwrap();
        assert!(!packets[0].has_payload());
        assert!(packets[0].payload(&data).is_empty());
    }
}
