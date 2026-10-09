pub(crate) mod mux;
pub(crate) mod nal;
pub(crate) mod packet;
pub(crate) mod pes;
pub(crate) mod psi;

/// Reads `buf[i]`, or 0 when out of range. Callers validate lengths first.
#[inline]
pub(crate) fn at(buf: &[u8], i: usize) -> u8 {
    buf.get(i).copied().unwrap_or(0)
}
