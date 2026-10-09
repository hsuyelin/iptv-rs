use crate::ts::at;

/// Byte ranges of one Annex B NAL unit inside a buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NalRange {
    /// Start of the start code (3 or 4 bytes).
    pub prefix_start: usize,
    /// First byte after the start code.
    pub start: usize,
    /// One past the last NAL byte.
    pub end: usize,
}

/// Finds the NAL units of `data[from..]`.
///
/// A start code is `00 00 01`, widened to `00 00 00 01` when three zeros precede the
/// one. A start code is only accepted when at least one byte follows it.
pub(crate) fn find_nals(data: &[u8], from: usize) -> Vec<NalRange> {
    let mut starts: Vec<(usize, usize)> = Vec::new();
    let mut zeros = 0usize;
    for (index, &byte) in data.iter().enumerate().skip(from) {
        match byte {
            0 => zeros += 1,
            1 if zeros >= 2 && index + 1 < data.len() => {
                let prefix_start = if zeros >= 3 { index - 3 } else { index - 2 };
                starts.push((prefix_start, index + 1));
                zeros = 0;
            }
            _ => zeros = 0,
        }
    }
    let mut nals = Vec::with_capacity(starts.len());
    for (position, &(prefix_start, start)) in starts.iter().enumerate() {
        let end = starts
            .get(position + 1)
            .map_or(data.len(), |&(next_prefix, _)| next_prefix);
        if start < end {
            nals.push(NalRange {
                prefix_start,
                start,
                end,
            });
        }
    }
    nals
}

/// H.264 NAL unit type of a non-empty NAL.
pub(crate) fn nal_type(nal: &[u8]) -> u8 {
    at(nal, 0) & 0x1f
}

/// Counts differing bytes, treating the length difference as differences.
pub(crate) fn count_byte_diff(left: &[u8], right: &[u8]) -> usize {
    let tail = left.len().abs_diff(right.len());
    tail + left.iter().zip(right).filter(|(a, b)| a != b).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_three_and_four_byte_start_codes() {
        let data = [
            0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x65, 9,
        ];
        let nals = find_nals(&data, 0);
        assert_eq!(
            nals,
            vec![
                NalRange {
                    prefix_start: 0,
                    start: 3,
                    end: 5
                },
                NalRange {
                    prefix_start: 5,
                    start: 9,
                    end: 12
                },
                NalRange {
                    prefix_start: 12,
                    start: 15,
                    end: 17
                },
            ]
        );
    }

    #[test]
    fn trailing_start_code_without_payload_is_ignored() {
        let data = [0, 0, 1, 0x65, 7, 0, 0, 1];
        let nals = find_nals(&data, 0);
        assert_eq!(nals.len(), 1);
        assert_eq!(nals[0].end, data.len());
    }

    #[test]
    fn byte_diff_counts_length_tail() {
        assert_eq!(count_byte_diff(&[1, 2, 3], &[1, 9]), 2);
        assert_eq!(count_byte_diff(&[], &[]), 0);
    }
}
