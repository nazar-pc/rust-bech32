// SPDX-License-Identifier: MIT

//! Provides an [`Hrp`] type that represents the human-readable part of a bech32 encoded string.
//!
//! > The human-readable part, which is intended to convey the type of data, or anything else that
//! > is relevant to the reader. This part MUST contain 1 to 83 US-ASCII characters, with each
//! > character having a value in the range [33-126]. HRP validity may be further restricted by
//! > specific applications.
//!
//! ref: [BIP-173](https://github.com/bitcoin/bips/blob/master/bip-0173.mediawiki#user-content-Bech32)

mod repr;

#[cfg(feature = "alloc")]
use alloc::alloc::{alloc, dealloc, handle_alloc_error};
#[cfg(all(feature = "alloc", not(feature = "std"), not(test)))]
use alloc::string::String;
#[cfg(feature = "alloc")]
use core::alloc::Layout;
use core::cmp::Ordering;
use core::fmt::{self, Write};
use core::iter::FusedIterator;
#[cfg(feature = "alloc")]
use core::ptr;
use core::{slice, str};

/// Number of bytes that are stored inline, without allocating.
const INLINE_LEN: usize = 7;

/// Maximum length of the human-readable part supported by this build.
///
/// BIP-173 defines it as 83 characters, but without the `alloc` feature nothing can be allocated,
/// so only the inline storage is available.
#[cfg(feature = "alloc")]
const MAX_LEN: usize = 83;
#[cfg(not(feature = "alloc"))]
const MAX_LEN: usize = INLINE_LEN;

// Defines HRP constants for the different bitcoin networks.
// You can also access these at `crate::hrp::BC` etc.
macro_rules! define_hrp_const {
    (
        #[$doc:meta]
        pub const $name:ident $v:literal;
    ) => {
        #[$doc]
        pub const $name: Hrp = Hrp::parse_unchecked_inline($v);
    };
}
define_hrp_const! {
    /// The human-readable part used by the Bitcoin mainnet network.
    pub const BC "bc";
}
define_hrp_const! {
    /// The human-readable part used by the Bitcoin testnet networks (testnet, signet).
    pub const TB "tb";
}
define_hrp_const! {
    /// The human-readable part used when running a Bitcoin regtest network.
    pub const BCRT "bcrt";
}

/// The human-readable part (human readable prefix before the '1' separator).
///
/// Occupies 8 bytes on targets with pointers up to 64 bits wide (and `size_of::<*mut u8>()` bytes
/// on wider ones): up to 7 bytes are stored inline, longer ones are stored on the heap, which
/// requires the `alloc` feature.
pub struct Hrp {
    repr: repr::Repr,
}

// SAFETY: `Hrp` owns its allocation exclusively and never hands out a way to mutate it through a
// shared reference.
unsafe impl Send for Hrp {}
// SAFETY: see above.
unsafe impl Sync for Hrp {}

impl Hrp {
    /// Parses the human-readable part checking it is valid as defined by [BIP-173].
    ///
    /// This does _not_ check that the `hrp` is an in-use HRP within Bitcoin (eg, "bc"), rather it
    /// checks that the HRP string is valid as per the specification in [BIP-173]:
    ///
    /// > The human-readable part, which is intended to convey the type of data, or anything else that
    /// > is relevant to the reader. This part MUST contain 1 to 83 US-ASCII characters, with each
    /// > character having a value in the range [33-126]. HRP validity may be further restricted by
    /// > specific applications.
    ///
    /// Without the `alloc` feature only human-readable parts of up to 7 characters are supported,
    /// longer ones fail with [`Error::TooLong`].
    ///
    /// [BIP-173]: <https://github.com/bitcoin/bips/blob/master/bip-0173.mediawiki>
    pub fn parse(hrp: &str) -> Result<Self, Error> {
        if hrp.is_empty() {
            return Err(Error::Empty);
        }
        if hrp.len() > MAX_LEN {
            return Err(Error::TooLong(hrp.len()));
        }

        let mut has_lower: bool = false;
        let mut has_upper: bool = false;
        for c in hrp.chars() {
            if !c.is_ascii() {
                return Err(Error::NonAsciiChar(c));
            }
            let b = c as u8; // cast OK as we just checked that c is an ASCII value

            // Valid subset of ASCII
            if !(33..=126).contains(&b) {
                return Err(Error::InvalidAsciiByte(b));
            }

            if b.is_ascii_lowercase() {
                if has_upper {
                    return Err(Error::MixedCase);
                }
                has_lower = true;
            } else if b.is_ascii_uppercase() {
                if has_lower {
                    return Err(Error::MixedCase);
                }
                has_upper = true;
            };
        }

        Ok(Self::new(hrp.as_bytes()))
    }

    /// Parses the human-readable part from an object which can be formatted.
    ///
    /// The formatted form of the object is subject to all the same rules as [`Self::parse`].
    /// This method is semantically equivalent to `Hrp::parse(&data.to_string())` but avoids
    /// allocating an intermediate string.
    pub fn parse_display<T: core::fmt::Display>(data: T) -> Result<Self, Error> {
        struct ByteFormatter {
            arr: [u8; MAX_LEN],
            index: usize,
            error: Option<Error>,
            has_lower: bool,
            has_upper: bool,
        }

        impl core::fmt::Write for ByteFormatter {
            fn write_str(&mut self, s: &str) -> fmt::Result {
                for ch in s.chars() {
                    let b = ch as u8; // cast ok, `b` unused until `ch` is checked to be ASCII

                    // Break after finding an error so that we report the first invalid
                    // character, not the last.
                    if !ch.is_ascii() {
                        self.error = Some(Error::NonAsciiChar(ch));
                        break;
                    } else if !(33..=126).contains(&b) {
                        self.error = Some(Error::InvalidAsciiByte(b));
                        break;
                    }

                    if ch.is_ascii_lowercase() {
                        if self.has_upper {
                            self.error = Some(Error::MixedCase);
                            break;
                        }
                        self.has_lower = true;
                    } else if ch.is_ascii_uppercase() {
                        if self.has_lower {
                            self.error = Some(Error::MixedCase);
                            break;
                        }
                        self.has_upper = true;
                    };
                }

                // However, an invalid length error will take priority over an
                // invalid character error.
                if self.index + s.len() > self.arr.len() {
                    self.error = Some(Error::TooLong(self.index + s.len()));
                } else {
                    // Only do the actual copy if we passed the index check.
                    self.arr[self.index..self.index + s.len()].copy_from_slice(s.as_bytes());
                }

                // Unconditionally update self.index so that in the case of a too-long
                // string, our error return will reflect the full length.
                self.index += s.len();
                Ok(())
            }
        }

        let mut byte_formatter = ByteFormatter {
            arr: [0; MAX_LEN],
            index: 0,
            error: None,
            has_lower: false,
            has_upper: false,
        };

        write!(byte_formatter, "{}", data).expect("custom Formatter cannot fail");
        if byte_formatter.index == 0 {
            Err(Error::Empty)
        } else if let Some(err) = byte_formatter.error {
            Err(err)
        } else {
            Ok(Self::new(&byte_formatter.arr[..byte_formatter.index]))
        }
    }

    /// Parses the human-readable part (see [`Hrp::parse`] for full docs).
    ///
    /// Does not check that `hrp` is valid according to BIP-173 but does check for valid ASCII
    /// values, replacing any invalid characters with `X`.
    ///
    /// See [`Hrp::parse_unchecked_inline`] for a version that can be used in const context.
    ///
    /// # Panics
    ///
    /// Will panic if the provided `hrp` string is longer than 83, or longer than 7 if the `alloc`
    /// feature is not enabled.
    pub fn parse_unchecked(hrp: &str) -> Self {
        assert!(hrp.len() <= MAX_LEN, "hrp is too long");

        let mut hrp = Self::new(hrp.as_bytes());
        for b in hrp.as_bytes_mut() {
            // Valid subset of ASCII
            if *b < 33 || *b > 126 {
                *b = b'X';
            }
        }
        hrp
    }

    /// Parses a human-readable part that is short enough to be stored inline (see [`Hrp::parse`]
    /// for full docs).
    ///
    /// Unlike [`Hrp::parse_unchecked`] this never allocates and can thus be used in const context,
    /// which makes it the way to define HRP constants.
    ///
    /// Does not check that `hrp` is valid according to BIP-173 but does check for valid ASCII
    /// values, replacing any invalid characters with `X`.
    ///
    /// # Panics
    ///
    /// Will panic if the provided `hrp` string is longer than 7.
    pub const fn parse_unchecked_inline(hrp: &str) -> Self {
        let hrp = hrp.as_bytes();
        // `assert!()` is not allowed in const functions on MSRV, an out of bounds index panics
        // in both const and runtime context instead.
        let _ = ["hrp is too long to be stored inline"][(hrp.len() > INLINE_LEN) as usize];

        let mut data = [0_u8; INLINE_LEN];
        let mut i = 0;
        // Funky code so we can be const.
        while i < hrp.len() {
            let b = hrp[i];
            // Valid subset of ASCII
            data[i] = if b < 33 || b > 126 { b'X' } else { b };
            i += 1;
        }

        Self::new_inline(data, hrp.len())
    }

    // Stores `bytes` inline or on the heap, depending on their length.
    //
    // # Panics
    //
    // Panics if `bytes` is longer than `MAX_LEN`.
    fn new(bytes: &[u8]) -> Self {
        assert!(bytes.len() <= MAX_LEN, "hrp is too long");

        if bytes.len() > INLINE_LEN {
            #[cfg(feature = "alloc")]
            {
                return Self::new_heap(bytes);
            }
            // `MAX_LEN` is `INLINE_LEN` without the `alloc` feature, hence the assert above has
            // already returned.
            #[cfg(not(feature = "alloc"))]
            unreachable!()
        }

        let mut data = [0_u8; INLINE_LEN];
        data[..bytes.len()].copy_from_slice(bytes);
        Self::new_inline(data, bytes.len())
    }

    // Stores the first `len` bytes of `data` inline.
    const fn new_inline(data: [u8; INLINE_LEN], len: usize) -> Self {
        Self { repr: repr::Repr::new_inline(data, len) }
    }

    // Copies `bytes` into a fresh heap allocation that stores the length in its first byte.
    #[cfg(feature = "alloc")]
    fn new_heap(bytes: &[u8]) -> Self {
        let layout = heap_layout(bytes.len());

        // SAFETY: layout is of non-zero size
        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            handle_alloc_error(layout);
        }

        // SAFETY: the allocation is `bytes.len() + 1` bytes long and freshly allocated, hence not
        // overlapping with `bytes`
        unsafe {
            ptr.write(bytes.len() as u8);
            ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.add(1), bytes.len());
        }

        Self { repr: repr::Repr::new_heap(ptr) }
    }

    // Returns `true` if the bytes are stored inline rather than on the heap.
    fn is_inline(&self) -> bool { self.repr.is_inline() }

    /// Returns this human-readable part as a lowercase string.
    #[cfg(feature = "alloc")]
    #[inline]
    pub fn to_lowercase(&self) -> String { self.lowercase_char_iter().collect() }

    /// Returns this human-readable part as bytes.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        if self.is_inline() {
            // SAFETY: just checked that the bytes are stored inline
            unsafe { self.repr.inline_bytes() }
        } else {
            // SAFETY: the bytes are stored on the heap, the allocation holds the length in its
            // first byte followed by that many bytes of data
            unsafe {
                let ptr = self.repr.heap_alloc_ptr();
                slice::from_raw_parts(ptr.add(1), usize::from(*ptr))
            }
        }
    }

    // Same as `as_bytes()`, but mutable.
    //
    // Callers must not break the ASCII or case invariants.
    #[inline]
    fn as_bytes_mut(&mut self) -> &mut [u8] {
        if self.is_inline() {
            // SAFETY: just checked that the bytes are stored inline
            unsafe { self.repr.inline_bytes_mut() }
        } else {
            // SAFETY: the bytes are stored on the heap, the allocation holds the length in its
            // first byte followed by that many bytes of data, and `&mut self` makes it exclusive
            unsafe {
                let ptr = self.repr.heap_alloc_ptr();
                slice::from_raw_parts_mut(ptr.add(1), usize::from(*ptr))
            }
        }
    }

    /// Returns this human-readable part as str.
    #[inline]
    pub fn as_str(&self) -> &str {
        str::from_utf8(self.as_bytes()).expect("we only store ASCII bytes")
    }

    /// Creates a byte iterator over the ASCII byte values (ASCII characters) of this HRP.
    ///
    /// If an uppercase HRP was parsed during object construction then this iterator will yield
    /// uppercase ASCII `char`s. For lowercase bytes see [`Self::lowercase_byte_iter`]
    #[inline]
    pub fn byte_iter(&self) -> ByteIter<'_> { ByteIter { iter: self.as_bytes().iter() } }

    /// Creates a character iterator over the ASCII characters of this HRP.
    ///
    /// If an uppercase HRP was parsed during object construction then this iterator will yield
    /// uppercase ASCII `char`s. For lowercase bytes see [`Self::lowercase_char_iter`].
    #[inline]
    pub fn char_iter(&self) -> CharIter<'_> { CharIter { iter: self.byte_iter() } }

    /// Creates a lowercase iterator over the byte values (ASCII characters) of this HRP.
    #[inline]
    pub fn lowercase_byte_iter(&self) -> LowercaseByteIter<'_> {
        LowercaseByteIter { iter: self.byte_iter() }
    }

    /// Creates a lowercase character iterator over the ASCII characters of this HRP.
    #[inline]
    pub fn lowercase_char_iter(&self) -> LowercaseCharIter<'_> {
        LowercaseCharIter { iter: self.lowercase_byte_iter() }
    }

    /// Returns the length (number of characters) of the human-readable part.
    ///
    /// Guaranteed to be between 1 and 83 inclusive, or between 1 and 7 inclusive without the
    /// `alloc` feature.
    #[inline]
    #[allow(clippy::len_without_is_empty)] // HRP is never empty.
    pub fn len(&self) -> usize { self.as_bytes().len() }

    /// Returns `true` if this HRP is valid according to the bips.
    ///
    /// [BIP-173] states that the HRP must be either "bc" or "tb".
    ///
    /// [BIP-173]: <https://github.com/bitcoin/bips/blob/master/bip-0173.mediawiki#user-content-Segwit_address_format>
    #[inline]
    pub fn is_valid_segwit(&self) -> bool {
        self.is_valid_on_mainnet() || self.is_valid_on_testnet()
    }

    /// Returns `true` if this HRP is valid on the Bitcoin network i.e., HRP is "bc".
    #[inline]
    pub fn is_valid_on_mainnet(&self) -> bool { *self == self::BC }

    /// Returns `true` if this HRP is valid on the Bitcoin testnet network i.e., HRP is "tb".
    #[inline]
    pub fn is_valid_on_testnet(&self) -> bool { *self == self::TB }

    /// Returns `true` if this HRP is valid on the Bitcoin signet network i.e., HRP is "tb".
    #[inline]
    pub fn is_valid_on_signet(&self) -> bool { *self == self::TB }

    /// Returns `true` if this HRP is valid on the Bitcoin regtest network i.e., HRP is "bcrt".
    #[inline]
    pub fn is_valid_on_regtest(&self) -> bool { *self == self::BCRT }
}

// Layout of the heap allocation holding `len` bytes of data prefixed with the length.
#[cfg(feature = "alloc")]
fn heap_layout(len: usize) -> Layout {
    Layout::from_size_align(len + 1, repr::HEAP_ALIGN).expect("hrp length is bounded by MAX_LEN")
}

impl Clone for Hrp {
    #[inline]
    fn clone(&self) -> Self {
        #[cfg(feature = "alloc")]
        if !self.is_inline() {
            return Self::new_heap(self.as_bytes());
        }

        // SAFETY: not stored on the heap, hence the inline storage is trivially copyable
        Self { repr: unsafe { self.repr.copy_inline() } }
    }
}

impl Drop for Hrp {
    #[inline]
    fn drop(&mut self) {
        #[cfg(feature = "alloc")]
        if !self.is_inline() {
            // SAFETY: the bytes are stored on the heap, the allocation was made with the same
            // layout and is not used afterwards
            unsafe {
                let ptr = self.repr.heap_alloc_ptr();
                dealloc(ptr, heap_layout(usize::from(*ptr)));
            }
        }
    }
}

impl fmt::Debug for Hrp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { fmt::Debug::fmt(self.as_str(), f) }
}

/// Displays the human-readable part.
///
/// If an uppercase HRP was parsed during object construction then the returned string will be
/// in uppercase also. For a lowercase string see `Self::to_lowercase`.
impl fmt::Display for Hrp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for c in self.char_iter() {
            f.write_char(c)?;
        }
        Ok(())
    }
}

/// Case insensitive comparison.
impl Ord for Hrp {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        self.lowercase_byte_iter().cmp(other.lowercase_byte_iter())
    }
}

/// Case insensitive comparison.
impl PartialOrd for Hrp {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}

/// Case insensitive comparison.
impl PartialEq for Hrp {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.lowercase_byte_iter().eq(other.lowercase_byte_iter())
    }
}

impl Eq for Hrp {}

impl core::hash::Hash for Hrp {
    #[inline]
    fn hash<H: core::hash::Hasher>(&self, h: &mut H) {
        self.len().hash(h);
        self.lowercase_byte_iter().for_each(|ch| ch.hash(h))
    }
}

/// Iterator over bytes (ASCII values) of the human-readable part.
///
/// ASCII byte values as they were initially parsed (i.e., in the original case).
pub struct ByteIter<'b> {
    iter: slice::Iter<'b, u8>,
}

impl Iterator for ByteIter<'_> {
    type Item = u8;
    #[inline]
    fn next(&mut self) -> Option<u8> { self.iter.next().copied() }
    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) { self.iter.size_hint() }
}

impl ExactSizeIterator for ByteIter<'_> {
    #[inline]
    fn len(&self) -> usize { self.iter.len() }
}

impl DoubleEndedIterator for ByteIter<'_> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> { self.iter.next_back().copied() }
}

impl FusedIterator for ByteIter<'_> {}

/// Iterator over ASCII characters of the human-readable part.
///
/// ASCII `char`s as they were initially parsed (i.e., in the original case).
pub struct CharIter<'b> {
    iter: ByteIter<'b>,
}

impl Iterator for CharIter<'_> {
    type Item = char;
    #[inline]
    fn next(&mut self) -> Option<char> { self.iter.next().map(Into::into) }
    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) { self.iter.size_hint() }
}

impl ExactSizeIterator for CharIter<'_> {
    #[inline]
    fn len(&self) -> usize { self.iter.len() }
}

impl DoubleEndedIterator for CharIter<'_> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> { self.iter.next_back().map(Into::into) }
}

impl FusedIterator for CharIter<'_> {}

/// Iterator over lowercase bytes (ASCII characters) of the human-readable part.
pub struct LowercaseByteIter<'b> {
    iter: ByteIter<'b>,
}

impl Iterator for LowercaseByteIter<'_> {
    type Item = u8;
    #[inline]
    fn next(&mut self) -> Option<u8> {
        self.iter.next().map(|b| if is_ascii_uppercase(b) { b | 32 } else { b })
    }
    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) { self.iter.size_hint() }
}

impl ExactSizeIterator for LowercaseByteIter<'_> {
    #[inline]
    fn len(&self) -> usize { self.iter.len() }
}

impl DoubleEndedIterator for LowercaseByteIter<'_> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        self.iter.next_back().map(|b| if is_ascii_uppercase(b) { b | 32 } else { b })
    }
}

impl FusedIterator for LowercaseByteIter<'_> {}

/// Iterator over lowercase ASCII characters of the human-readable part.
pub struct LowercaseCharIter<'b> {
    iter: LowercaseByteIter<'b>,
}

impl Iterator for LowercaseCharIter<'_> {
    type Item = char;
    #[inline]
    fn next(&mut self) -> Option<char> { self.iter.next().map(Into::into) }
    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) { self.iter.size_hint() }
}

impl ExactSizeIterator for LowercaseCharIter<'_> {
    #[inline]
    fn len(&self) -> usize { self.iter.len() }
}

impl DoubleEndedIterator for LowercaseCharIter<'_> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> { self.iter.next_back().map(Into::into) }
}

impl FusedIterator for LowercaseCharIter<'_> {}

fn is_ascii_uppercase(b: u8) -> bool { (65..=90).contains(&b) }

/// Errors encountered while checking the human-readable part as defined by [BIP-173].
///
/// [BIP-173]: <https://github.com/bitcoin/bips/blob/master/bip-0173.mediawiki#user-content-Bech32>
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The human-readable part is too long.
    TooLong(usize),
    /// The human-readable part is empty.
    Empty,
    /// Found a non-ASCII character.
    NonAsciiChar(char),
    /// Byte value not within acceptable US-ASCII range.
    InvalidAsciiByte(u8),
    /// The human-readable part cannot mix upper and lower case.
    MixedCase,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Self::TooLong(len) => {
                write!(f, "hrp is too long, found {} characters, must be <= {}", len, MAX_LEN)
            }
            Self::Empty => write!(f, "hrp is empty, must have at least 1 character"),
            Self::NonAsciiChar(c) => write!(f, "found non-ASCII character: {}", c),
            Self::InvalidAsciiByte(b) => write!(f, "byte value is not valid US-ASCII: \'{:x}\'", b),
            Self::MixedCase => write!(f, "hrp cannot mix upper and lower case"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match *self {
            Self::TooLong(_)
            | Self::Empty
            | Self::NonAsciiChar(_)
            | Self::InvalidAsciiByte(_)
            | Self::MixedCase => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use core::hash::{Hash, Hasher};
    use core::mem::{align_of, size_of};

    use super::*;

    macro_rules! check_parse_ok {
        ($($(#[$attr:meta])* $test_name:ident, $hrp:literal);* $(;)?) => {
            $(
                #[test]
                $(#[$attr])*
                fn $test_name() {
                    assert!(Hrp::parse($hrp).is_ok());
                    assert!(Hrp::parse_display($hrp).is_ok());
                }
            )*
        }
    }
    check_parse_ok! {
        parse_ok_0, "a";
        parse_ok_1, "A";
        parse_ok_2, "abcdefg";
        parse_ok_3, "ABCDEFG";
        #[cfg(feature = "alloc")]
        parse_ok_4, "abc123def";
        #[cfg(feature = "alloc")]
        parse_ok_5, "ABC123DEF";
        #[cfg(feature = "alloc")]
        parse_ok_6, "!\"#$%&'()*+,-./";
        #[cfg(feature = "alloc")]
        parse_ok_7, "1234567890";
    }

    macro_rules! check_parse_err {
        ($($test_name:ident, $hrp:literal);* $(;)?) => {
            $(
                #[test]
                fn $test_name() {
                    assert!(Hrp::parse($hrp).is_err());
                    assert!(Hrp::parse_display($hrp).is_err());
                }
            )*
        }
    }
    check_parse_err! {
        parse_err_0, "has-capitals-aAbB";
        parse_err_1, "has-value-out-of-range-∈∈∈∈∈∈∈∈";
        parse_err_2, "toolongtoolongtoolongtoolongtoolongtoolongtoolongtoolongtoolongtoolongtoolongtoolongtoolongtoolong";
        parse_err_3, "has spaces in it";
    }

    macro_rules! check_iter {
        ($($(#[$attr:meta])* $test_name:ident, $hrp:literal, $len:literal);* $(;)?) => {
            $(
                #[test]
                $(#[$attr])*
                fn $test_name() {
                    let hrp = Hrp::parse($hrp).expect(&format!("failed to parse hrp {}", $hrp));

                    // Test ByteIter forwards.
                    for (got, want) in hrp.byte_iter().zip($hrp.bytes()) {
                        assert_eq!(got, want);
                    }

                    // Test ByteIter backwards.
                    for (got, want) in hrp.byte_iter().rev().zip($hrp.bytes().rev()) {
                        assert_eq!(got, want);
                    }

                    // Test exact sized works.
                    let mut iter = hrp.byte_iter();
                    for i in 0..$len {
                        assert_eq!(iter.len(), $len - i);
                        let _ = iter.next();
                    }
                    assert!(iter.next().is_none());

                    // Test CharIter forwards.
                    let iter = hrp.char_iter();
                    assert_eq!($hrp.to_string(), iter.collect::<String>());

                    for (got, want) in hrp.char_iter().zip($hrp.chars()) {
                        assert_eq!(got, want);
                    }

                    // Test CharIter backwards.
                    for (got, want) in hrp.char_iter().rev().zip($hrp.chars().rev()) {
                        assert_eq!(got, want);
                    }

                    // Test LowercaseCharIter forwards (implicitly tests LowercaseByteIter)
                    for (got, want) in hrp.lowercase_char_iter().zip($hrp.chars().map(|c| c.to_ascii_lowercase())) {
                        assert_eq!(got, want);
                    }

                    // Test LowercaseCharIter backwards (implicitly tests LowercaseByteIter)
                    for (got, want) in hrp.lowercase_char_iter().rev().zip($hrp.chars().rev().map(|c| c.to_ascii_lowercase())) {
                        assert_eq!(got, want);
                    }
                }
            )*
        }
    }
    check_iter! {
        char_0, "abc", 3;
        char_1, "ABC", 3;
        char_2, "abc123", 6;
        char_3, "ABC123", 6;
        #[cfg(feature = "alloc")]
        char_4, "abc123def", 9;
        #[cfg(feature = "alloc")]
        char_5, "ABC123DEF", 9;
    }

    #[test]
    fn representation_is_compact() {
        let ptr_len = size_of::<*mut u8>();
        let expected_len = if ptr_len > INLINE_LEN + 1 { ptr_len } else { INLINE_LEN + 1 };

        assert_eq!(size_of::<Hrp>(), expected_len);
        assert_eq!(align_of::<Hrp>(), align_of::<*mut u8>());
    }

    #[test]
    fn hrp_is_send_and_sync() {
        fn assert_send_and_sync<T: Send + Sync>() {}
        assert_send_and_sync::<Hrp>();
    }

    #[test]
    fn inline_storage_is_used_up_to_the_capacity() {
        for len in 1..=INLINE_LEN {
            let s = &"abcdefg"[..len];
            let hrp = Hrp::parse(s).expect("valid hrp");
            assert!(hrp.is_inline());
            assert_eq!(hrp.len(), len);
            assert_eq!(hrp.as_str(), s);
        }
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn heap_storage_is_used_above_the_inline_capacity() {
        let s = "a".repeat(MAX_LEN);

        for len in INLINE_LEN + 1..=MAX_LEN {
            let s = &s[..len];
            let hrp = Hrp::parse(s).expect("valid hrp");
            assert!(!hrp.is_inline());
            assert_eq!(hrp.len(), len);
            assert_eq!(hrp.as_str(), s);
            assert_eq!(hrp.as_bytes(), s.as_bytes());
        }
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn clone_is_independent_of_the_original() {
        let s = "averylonghumanreadablepart";
        let cloned = {
            let hrp = Hrp::parse(s).expect("valid hrp");
            let cloned = hrp.clone();
            assert_eq!(hrp, cloned);
            cloned
        };

        assert_eq!(cloned.as_str(), s);
    }

    #[cfg(not(feature = "alloc"))]
    #[test]
    fn long_hrp_is_rejected_without_alloc() {
        assert_eq!(Hrp::parse("abcdefgh").unwrap_err(), Error::TooLong(8));
        assert_eq!(Hrp::parse_display("abcdefgh").unwrap_err(), Error::TooLong(8));
    }

    #[test]
    #[should_panic]
    fn parse_unchecked_inline_rejects_long_hrp() {
        let _ = Hrp::parse_unchecked_inline("abcdefgh");
    }

    #[test]
    fn parse_unchecked_inline_replaces_invalid_bytes() {
        assert_eq!(Hrp::parse_unchecked_inline("a c").as_str(), "aXc");
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn hrp_consts() {
        use crate::primitives::hrp::{BC, BCRT, TB};
        assert_eq!(BC, Hrp::parse_unchecked("bc"));
        assert_eq!(TB, Hrp::parse_unchecked("tb"));
        assert_eq!(BCRT, Hrp::parse_unchecked("bcrt"));
    }

    #[test]
    #[cfg(feature = "alloc")]
    fn as_str() {
        let s = "arbitraryhrp";
        let hrp = Hrp::parse_unchecked(s);
        assert_eq!(hrp.as_str(), s);
    }

    #[test]
    #[cfg(feature = "alloc")]
    fn as_bytes() {
        let s = "arbitraryhrp";
        let hrp = Hrp::parse_unchecked(s);
        assert_eq!(hrp.as_bytes(), s.as_bytes());
    }

    #[test]
    fn parse_display() {
        let hrp = Hrp::parse_display(format_args!("{}_{}", 123, "abc")).unwrap();
        assert_eq!(hrp.as_str(), "123_abc");
    }

    #[test]
    #[cfg(feature = "alloc")]
    fn parse_display_long() {
        let hrp = Hrp::parse_display(format_args!("{:083}", 1)).unwrap();
        assert_eq!(
            hrp.as_str(),
            "00000000000000000000000000000000000000000000000000000000000000000000000000000000001"
        );

        assert_eq!(Hrp::parse_display(format_args!("{:084}", 1)), Err(Error::TooLong(84)),);

        assert_eq!(
            Hrp::parse_display(format_args!("{:83}", 1)),
            Err(Error::InvalidAsciiByte(b' ')),
        );
    }

    #[test]
    fn parse_non_ascii() {
        assert_eq!(Hrp::parse("❤").unwrap_err(), Error::NonAsciiChar('❤'));
    }

    #[test]
    fn parse_display_non_ascii() {
        assert_eq!(Hrp::parse_display("❤").unwrap_err(), Error::NonAsciiChar('❤'));
    }

    #[test]
    fn parse_display_returns_first_error() {
        assert_eq!(Hrp::parse_display("❤ ").unwrap_err(), Error::NonAsciiChar('❤'));
    }

    // This test shows that the error does not contain heart.
    #[test]
    fn parse_display_iterates_chars() {
        assert_eq!(Hrp::parse_display(" ❤").unwrap_err(), Error::InvalidAsciiByte(b' '));
        assert_eq!(Hrp::parse_display("_❤").unwrap_err(), Error::NonAsciiChar('❤'));
    }

    #[test]
    #[cfg(feature = "alloc")]
    fn display_and_lowercase() {
        let hrp = Hrp::parse_unchecked("test");
        assert_eq!(hrp.to_string(), "test");

        let hrp = Hrp::parse_unchecked("ABC");
        assert_eq!(hrp.to_lowercase(), "abc");
        let hrp2 = Hrp::parse_unchecked("abc");
        assert_eq!(hrp2.to_lowercase(), "abc");
    }

    #[test]
    fn is_valid_on_networks() {
        let bc = Hrp::parse_unchecked("bc");
        assert!(bc.is_valid_on_mainnet());
        assert!(!bc.is_valid_on_testnet());
        assert!(!bc.is_valid_on_signet());
        assert!(!bc.is_valid_on_regtest());

        let tb = Hrp::parse_unchecked("tb");
        assert!(!tb.is_valid_on_mainnet());
        assert!(tb.is_valid_on_testnet());
        assert!(tb.is_valid_on_signet());
        assert!(!tb.is_valid_on_regtest());

        let bcrt = Hrp::parse_unchecked("bcrt");
        assert!(!bcrt.is_valid_on_mainnet());
        assert!(!bcrt.is_valid_on_testnet());
        assert!(!bcrt.is_valid_on_signet());
        assert!(bcrt.is_valid_on_regtest());
    }

    #[test]
    fn ordering_and_hash() {
        let a = Hrp::parse_unchecked("a");
        let b = Hrp::parse_unchecked("b");
        assert_eq!(a.partial_cmp(&b), Some(core::cmp::Ordering::Less));

        struct Simple(u64);
        impl Hasher for Simple {
            fn finish(&self) -> u64 { self.0 }
            fn write(&mut self, bytes: &[u8]) {
                for &b in bytes {
                    self.0 = self.0.wrapping_mul(31).wrapping_add(b as u64);
                }
            }
        }

        let x = Hrp::parse_unchecked("bc");
        let y = Hrp::parse_unchecked("bc");
        let mut h1 = Simple(0);
        let mut h2 = Simple(0);
        x.hash(&mut h1);
        y.hash(&mut h2);
        assert_ne!(h1.finish(), 0);
    }

    #[test]
    fn iterator_next_back() {
        let hrp = Hrp::parse_unchecked("abc");
        let mut char_iter = hrp.char_iter();
        assert_eq!(char_iter.next_back(), Some('c'));

        let mut lower_char = hrp.lowercase_char_iter();
        assert_eq!(lower_char.next_back(), Some('c'));

        let hrp_upper = Hrp::parse_unchecked("ABC");
        let mut lower_byte = hrp_upper.lowercase_byte_iter();
        assert_eq!(lower_byte.next_back(), Some(b'c'));
        assert_eq!(lower_byte.next_back(), Some(b'b'));
        assert_eq!(lower_byte.next_back(), Some(b'a'));

        let mut lower_char_upper = hrp_upper.lowercase_char_iter();
        assert_eq!(lower_char_upper.next_back(), Some('c'));
    }

    #[test]
    fn char_iter_size_hints() {
        let hrp = Hrp::parse_unchecked("test");
        let iter = hrp.char_iter();
        assert_eq!(iter.len(), 4);
        assert_eq!(iter.size_hint(), (4, Some(4)));

        let lower = hrp.lowercase_char_iter();
        assert_eq!(lower.len(), 4);
        assert_eq!(lower.size_hint(), (4, Some(4)));
    }

    #[test]
    fn is_ascii_uppercase_with_non_letter_bytes() {
        // This ensures the lowercase iterator handles non-letter uppercase-range bytes correctly
        let hrp = Hrp::parse_unchecked("A]B");
        let lower_bytes: Vec<u8> = hrp.lowercase_byte_iter().collect();
        assert_eq!(lower_bytes, vec![b'a', b']', b'b']);
    }

    #[test]
    fn error_display_non_empty() {
        let e = Error::Empty;
        assert!(!e.to_string().is_empty());
    }

    #[test]
    fn parse_unchecked_replaces_invalid_bytes() {
        // Bytes < 33 or > 126 are replaced with 'X'
        let hrp = Hrp::parse_unchecked("a\x01b");
        assert_eq!(hrp.as_bytes()[1], b'X');
        // Boundary: byte 33 (!) is valid, byte 126 (~) is valid, byte 127 is invalid
        let hrp = Hrp::parse_unchecked("!\x7f~");
        assert_eq!(hrp.as_bytes()[0], b'!');
        assert_eq!(hrp.as_bytes()[1], b'X');
        assert_eq!(hrp.as_bytes()[2], b'~');
    }

    #[test]
    fn test_parse_display_mixed_case_across_boundaries() {
        let result_lowercase_then_uppercase_static =
            Hrp::parse_display(format_args!("{}{}", "abc", "DEF"));
        assert_eq!(
            result_lowercase_then_uppercase_static.unwrap_err(),
            Error::MixedCase,
            "Failed to detect mixed case when lowercase precedes uppercase across boundaries"
        );

        let lower_abc = "abc".to_string();
        let upper_def = "DEF".to_string();
        let result_lowercase_then_uppercase_heap =
            Hrp::parse_display(format_args!("{}{}", lower_abc, upper_def));
        assert_eq!(
            result_lowercase_then_uppercase_heap.unwrap_err(),
            Error::MixedCase,
            "Failed to detect mixed case when lowercase precedes uppercase across boundaries"
        );
    }

    #[test]
    fn hash_matches_case_insensitive_eq() {
        // `Hrp`'s `Eq`/`Ord` are case-insensitive, so "BC" and "bc" are equal.
        // The `Hash`/`Eq` contract then requires them to hash identically, but
        // the `Hash` impl hashes the raw `buf` in its original case, so equal
        // values hash differently and break hash-based collections.
        struct Simple(u64);
        impl Hasher for Simple {
            fn finish(&self) -> u64 { self.0 }
            fn write(&mut self, bytes: &[u8]) {
                for &b in bytes {
                    self.0 = self.0.wrapping_mul(31).wrapping_add(b as u64);
                }
            }
        }

        let upper = Hrp::parse_unchecked("BC");
        let lower = Hrp::parse_unchecked("bc");
        assert_eq!(upper, lower);

        let mut h1 = Simple(0);
        let mut h2 = Simple(0);
        upper.hash(&mut h1);
        lower.hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish());
    }
}
