//! PETSCII, the Commodore's character set, and plain ASCII.
//!
//! File names, paths and disk names are given to this crate as PETSCII
//! bytes, exactly as they're stored on the disk, so every file can be
//! named, even one whose name holds graphics or control characters (as
//! "directory art" disks do). PETSCII and ASCII agree on the space,
//! digits, capital letters and most punctuation (`$20`–`$5D`), so an
//! ordinary name such as `"GAMES/PACMAN"` can be passed as a Rust string as
//! it is. Lower-case Rust letters are *not* the same: `"hello"` is five
//! PETSCII graphics characters. [`ascii_to_petscii`] converts.
//!
//! A Commodore shows PETSCII in one of two character sets, and the same
//! byte looks different in each:
//!
//! | PETSCII | Upper case / graphics (power-on) | Lower / upper case |
//! |---|---|---|
//! | `$41`–`$5A` | `A`–`Z` | `a`–`z` |
//! | `$61`–`$7A`, `$C1`–`$DA` | graphics characters | `A`–`Z` |
//! | `$5C` | `£` | `£` |
//! | `$5E` | `↑` (shown here as `^`) | `↑` (`^`) |
//! | `$5F` | `←` (shown here as `_`) | `←` (`_`) |
//! | `$A0` | shifted space (shown as a space) | shifted space |
//! | other bytes | graphics and control characters | the same |
//!
//! The `_lower` functions use the lower/upper case set; the others the
//! power-on set, which is how a directory listing first appears.

use crate::error::{Error, Result};

/// The shifted space, which pads names and disk names to 16 bytes.
pub const SHIFTED_SPACE: u8 = 0xA0;

/// ASCII text as PETSCII, as typed in the power-on character set:
/// letters of either case become `$41`–`$5A` (that set has only
/// capitals), `^` becomes `↑` (`$5E`) and `_` becomes `←` (`$5F`); the
/// rest of printable ASCII is kept as it is. Fails with
/// [`Error::BadName`] on a character PETSCII doesn't have: `` ` ``,
/// `\`, `{`, `|`, `}`, `~`, and anything that isn't printable ASCII.
///
/// ```
/// use cbm_disk::petscii::ascii_to_petscii;
///
/// assert_eq!(ascii_to_petscii("Hello 64")?, b"HELLO 64");
/// # Ok::<(), cbm_disk::Error>(())
/// ```
pub fn ascii_to_petscii(text: &str) -> Result<Vec<u8>> {
    text.chars()
        .map(|c| match c {
            'a'..='z' => Ok(c.to_ascii_uppercase() as u8),
            ' '..='[' | ']'..='_' => Ok(c as u8),
            _ => Err(no_petscii(text, c)),
        })
        .collect()
}

/// ASCII text as PETSCII, as typed in the lower/upper case character
/// set: lower-case letters become `$41`–`$5A` and capitals `$C1`–`$DA`
/// (as the C64 keyboard stores them), otherwise as
/// [`ascii_to_petscii`].
///
/// ```
/// use cbm_disk::petscii::ascii_to_petscii_lower;
///
/// assert_eq!(ascii_to_petscii_lower("Hi")?, [0xC8, 0x49]);
/// # Ok::<(), cbm_disk::Error>(())
/// ```
pub fn ascii_to_petscii_lower(text: &str) -> Result<Vec<u8>> {
    text.chars()
        .map(|c| match c {
            'a'..='z' => Ok(c.to_ascii_uppercase() as u8),
            'A'..='Z' => Ok(c as u8 + 0x80),
            ' '..='[' | ']'..='_' => Ok(c as u8),
            _ => Err(no_petscii(text, c)),
        })
        .collect()
}

/// PETSCII bytes as ASCII text to show, as the power-on character set
/// draws them: `$41`–`$5A` are capitals, `↑` is `^`, `←` is `_`, the
/// shifted space is a space, and every character ASCII has no match for
/// (graphics, `£`, control codes) is `?`.
pub fn petscii_to_ascii(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| upper(b).unwrap_or('?')).collect()
}

/// PETSCII bytes as ASCII text to show, as the lower/upper case
/// character set draws them: `$41`–`$5A` are lower case, `$61`–`$7A`
/// and `$C1`–`$DA` are capitals, otherwise as [`petscii_to_ascii`].
///
/// ```
/// use cbm_disk::petscii::petscii_to_ascii_lower;
///
/// assert_eq!(petscii_to_ascii_lower(&[0xC8, 0x49]), "Hi");
/// ```
pub fn petscii_to_ascii_lower(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| match b {
            0x41..=0x5A => (b + 0x20) as char,
            0x61..=0x7A => (b - 0x20) as char,
            0xC1..=0xDA => (b - 0x80) as char,
            _ => upper(b).unwrap_or('?'),
        })
        .collect()
}

fn upper(b: u8) -> Option<char> {
    match b {
        0x20..=0x5B | 0x5D..=0x5F => Some(b as char),
        SHIFTED_SPACE => Some(' '),
        _ => None,
    }
}

fn no_petscii(text: &str, c: char) -> Error {
    Error::BadName(format!("{text:?}: {c:?} has no PETSCII character"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_both_ways() {
        assert_eq!(ascii_to_petscii("HELLO, 64!").unwrap(), b"HELLO, 64!");
        assert_eq!(ascii_to_petscii("hello").unwrap(), b"HELLO");
        assert_eq!(ascii_to_petscii("^_").unwrap(), [0x5E, 0x5F]);
        for bad in ["a`b", "\\", "{", "|", "}", "~", "\t", "é"] {
            assert!(
                matches!(ascii_to_petscii(bad), Err(Error::BadName(_))),
                "{bad:?}"
            );
        }
        assert_eq!(petscii_to_ascii(b"HELLO"), "HELLO");
        assert_eq!(petscii_to_ascii(&[0x5C, 0x61, 0xC1, 0x0D, 0xA0]), "???? ");
        assert_eq!(petscii_to_ascii_lower(b"HELLO"), "hello");
        assert_eq!(petscii_to_ascii_lower(&[0x61, 0xDA]), "AZ");
        // Everything printable goes there and back unchanged.
        for c in (b' '..=b'_').filter(|&c| c != b'\\') {
            let s = (c as char).to_string();
            assert_eq!(petscii_to_ascii(&ascii_to_petscii(&s).unwrap()), s);
        }
        let mixed = "Mixed Case 123";
        let p = ascii_to_petscii_lower(mixed).unwrap();
        assert_eq!(petscii_to_ascii_lower(&p), mixed);
    }
}
