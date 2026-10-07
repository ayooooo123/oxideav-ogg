//! Timing of the Ogg timed-text mappings, Kate
//! (<https://wiki.xiph.org/OggKate>, libkate's `doc/format.txt`) and CMML
//! (<https://wiki.xiph.org/CMML>). Both pack two times into a granule
//! position, split at a shift their identification header declares; the
//! time base is the header's granule rate.
//! - Kate: the upper bits are the start of the earliest event still active
//!   (the base), the lower bits the offset from it to the start of the
//!   packet's event, so a packet starts at base + offset (libkate's
//!   `kate_granule_time`).
//! - CMML: the upper bits are the clip's own time, the lower bits the
//!   previous clip's.

use oxideav_core::TimeBase;

/// How a timed-text stream's granules map to packet times.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Granule {
    Kate { shift: u8 },
    Cmml { shift: u8 },
}

impl Granule {
    /// The time (in granule-rate ticks) a granule position names.
    pub(crate) fn time(self, granule: i64) -> i64 {
        let (shift, kate) = match self {
            Granule::Kate { shift } => (u32::from(shift), true),
            Granule::Cmml { shift } => (u32::from(shift), false),
        };
        let g = granule as u64;
        let base = g.checked_shr(shift).unwrap_or(0);
        let offset = if kate { g - base.checked_shl(shift).unwrap_or(0) } else { 0 };
        base.saturating_add(offset).min(i64::MAX as u64) as i64
    }
}

/// A rate `num`/`den` ticks per second as a time base, if usable.
fn rate(num: u64, den: u64) -> Option<TimeBase> {
    (num > 0 && den > 0 && num <= i64::MAX as u64 && den <= i64::MAX as u64)
        .then(|| TimeBase::new(den as i64, num as i64))
}

/// Kate ID header (64 bytes): the granule shift at byte 15, the granule
/// rate numerator and denominator as little-endian 32-bit values at bytes
/// 24 and 28.
pub(crate) fn kate(id: &[u8]) -> Option<(TimeBase, Granule)> {
    let id = id.get(..64)?;
    let le32 = |at: usize| u64::from(u32::from_le_bytes([id[at], id[at + 1], id[at + 2], id[at + 3]]));
    Some((rate(le32(24), le32(28))?, Granule::Kate { shift: id[15] }))
}

/// Kate's header count: byte 11 of the ID header, the ID header included.
pub(crate) fn kate_headers(id: &[u8]) -> Option<usize> {
    id.get(11).map(|&n| usize::from(n)).filter(|&n| n > 0)
}

/// CMML ident header (29 bytes): version at bytes 8 and 10, the granule
/// rate numerator and denominator as little-endian 64-bit values at bytes
/// 12 and 20, the granule shift at byte 28.
pub(crate) fn cmml(ident: &[u8]) -> Option<(TimeBase, Granule)> {
    let ident = ident.get(..29)?;
    let le64 = |at: usize| u64::from_le_bytes(ident[at..at + 8].try_into().expect("8 bytes"));
    Some((rate(le64(12), le64(20))?, Granule::Cmml { shift: ident[28] }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kate_adds_the_offset_to_the_base_and_cmml_takes_the_upper_bits() {
        let g = (500i64 << 32) | 2000;
        assert_eq!(Granule::Kate { shift: 32 }.time(g), 2500);
        assert_eq!(Granule::Cmml { shift: 32 }.time(g), 500);
        assert_eq!(Granule::Kate { shift: 0 }.time(7), 7);
        // Shifts past the width degrade instead of panicking.
        assert_eq!(Granule::Cmml { shift: 200 }.time(i64::MAX), 0);
        assert_eq!(Granule::Kate { shift: 64 }.time(5), 5);
    }
}
