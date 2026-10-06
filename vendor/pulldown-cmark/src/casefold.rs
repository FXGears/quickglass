//! Unicode case-insensitive strings for link and footnote labels.
//!
//! QuickGlass patch: replaces the `unicase` crate. Folding is identical (the
//! table in `casefold_table.rs` is generated from unicase 2.9.0 by
//! `casefold-gen`, and a test checks every code point), but the table is
//! ~3.5 KB of runs instead of unicase's ~33 KB of match arms and fold data.

use core::hash::{Hash, Hasher};

use crate::casefold_table::{MULTI_CHARS, MULTI_END, MULTI_KEY, RUN_DELTA, RUN_LEN_STRIDE, RUN_START};

/// Full Unicode case folding of one char: one to three chars.
#[derive(Clone, Copy)]
enum Fold {
    One(char),
    Many(&'static [char]),
}

/// Folds one char, as `unicase::UniCase` does.
fn fold(c: char) -> Fold {
    let cp = c as u32;
    if cp < 0x80 {
        return Fold::One(c.to_ascii_lowercase());
    }
    if let Ok(i) = MULTI_KEY.binary_search(&cp) {
        let start = if i == 0 { 0 } else { MULTI_END[i - 1] as usize };
        return Fold::Many(&MULTI_CHARS[start..MULTI_END[i] as usize]);
    }
    let run = match RUN_START.binary_search(&cp) {
        Ok(i) => i,
        Err(0) => return Fold::One(c),
        Err(i) => i - 1,
    };
    let offset = cp - RUN_START[run];
    let len = u32::from(RUN_LEN_STRIDE[run] >> 1);
    let stride = u32::from(RUN_LEN_STRIDE[run] & 1) + 1;
    if offset % stride != 0 || offset / stride >= len {
        return Fold::One(c);
    }
    // Generated deltas always land on a valid scalar value.
    Fold::One(char::from_u32((cp as i32 + RUN_DELTA[run]) as u32).unwrap_or(c))
}

/// Folded chars of a string, in order.
fn folded(s: &str) -> impl Iterator<Item = char> + '_ {
    s.chars().flat_map(|c| {
        let (one, many) = match fold(c) {
            Fold::One(one) => (Some(one), &[][..]),
            Fold::Many(many) => (None, many),
        };
        one.into_iter().chain(many.iter().copied())
    })
}

/// A string compared and hashed by its Unicode case folding.
///
/// Same name and constructor as `unicase::UniCase`, so call sites are unchanged.
#[derive(Clone, Debug)]
pub(crate) struct UniCase<S>(S);

impl<S> UniCase<S> {
    pub(crate) fn new(s: S) -> Self {
        UniCase(s)
    }
}

impl<S: AsRef<str>> AsRef<str> for UniCase<S> {
    fn as_ref(&self) -> &str {
        self.0.as_ref()
    }
}

impl<S: AsRef<str>> PartialEq for UniCase<S> {
    fn eq(&self, other: &Self) -> bool {
        folded(self.0.as_ref()).eq(folded(other.0.as_ref()))
    }
}

impl<S: AsRef<str>> Eq for UniCase<S> {}

impl<S: AsRef<str>> Hash for UniCase<S> {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        let mut buf = [0; 4];
        for c in folded(self.0.as_ref()) {
            hasher.write(c.encode_utf8(&mut buf).as_bytes());
        }
        // Prefix-freedom, so ("ab", "c") and ("a", "bc") hash apart in tuples.
        hasher.write_u8(0xFF);
    }
}

#[cfg(test)]
mod tests {
    use super::{folded, UniCase};

    /// Every code point folds exactly as unicase folds it.
    #[test]
    fn matches_unicase_everywhere() {
        let mut checked = 0;
        for cp in 0..=0x10FFFFu32 {
            let Some(c) = char::from_u32(cp) else { continue };
            let s = c.to_string();
            let ours: String = folded(&s).collect();
            let theirs = unicase::UniCase::unicode(s.as_str()).to_folded_case();
            assert_eq!(ours, theirs, "U+{cp:04X}");
            checked += 1;
        }
        assert_eq!(checked, 0x10FFFF + 1 - 0x800);
    }

    #[test]
    fn label_equality() {
        assert_eq!(UniCase::new("Foo BAR"), UniCase::new("foo bar"));
        assert_eq!(UniCase::new("Maße"), UniCase::new("MASSE"));
        assert_eq!(UniCase::new("ﬂour"), UniCase::new("flour"));
        assert_eq!(UniCase::new("\u{212A}"), UniCase::new("k")); // Kelvin sign
        assert_ne!(UniCase::new("foo"), UniCase::new("foe"));
        assert_ne!(UniCase::new("ab"), UniCase::new("abc"));
    }
}
