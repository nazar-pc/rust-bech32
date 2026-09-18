// SPDX-License-Identifier: MIT

//! Storage of [`Hrp`](super::Hrp), hiding the layout differences between little- and big-endian
//! targets.
//!
//! Either up to [`INLINE_LEN`] bytes stored inline or a pointer to a heap allocation. The two are
//! told apart by the least significant bit of the tag byte, which is placed where it aliases the
//! least significant byte of the pointer: heap allocations are aligned to [`HEAP_ALIGN`] bytes and
//! thus have that bit cleared, while the inline representation sets it.

use core::mem::size_of;

use super::INLINE_LEN;

/// Alignment of the heap allocation.
///
/// Anything above one byte works, all that is needed is a least significant bit that is known to
/// be zero and thus distinguishes a heap pointer from inline storage.
#[cfg(feature = "alloc")]
pub(super) const HEAP_ALIGN: usize = 2;

/// Size of a pointer on the target platform.
const PTR_LEN: usize = size_of::<*mut u8>();

/// Size of [`Repr`].
///
/// One byte is spent on the tag, leaving [`INLINE_LEN`] bytes for inline storage. On platforms
/// with pointers wider than that the pointer decides the size instead.
const REPR_LEN: usize = if PTR_LEN > INLINE_LEN + 1 { PTR_LEN } else { INLINE_LEN + 1 };

/// Padding that fills [`InlineRepr`] up to [`REPR_LEN`] bytes.
const PAD_LEN: usize = REPR_LEN - (INLINE_LEN + 1);

/// Padding that fills [`HeapRepr`] up to [`REPR_LEN`] bytes.
const PTR_PAD_LEN: usize = REPR_LEN - PTR_LEN;

// The tag byte goes first on little-endian targets and last on big-endian ones, which is where
// the least significant byte of the pointer ends up in both cases.
#[cfg(target_endian = "little")]
#[derive(Clone, Copy)]
#[repr(C)]
struct InlineRepr {
    tag: u8,
    data: [u8; INLINE_LEN],
    _pad: [u8; PAD_LEN],
}

#[cfg(target_endian = "big")]
#[derive(Clone, Copy)]
#[repr(C)]
struct InlineRepr {
    data: [u8; INLINE_LEN],
    _pad: [u8; PAD_LEN],
    tag: u8,
}

// Mirrors `InlineRepr`: the pointer goes first on little-endian targets and last on big-endian
// ones, so that its least significant byte lands at the offset of `InlineRepr::tag`.
#[cfg(target_endian = "little")]
#[derive(Clone, Copy)]
#[repr(C)]
struct HeapRepr {
    ptr: *mut u8,
    _pad: [u8; PTR_PAD_LEN],
}

#[cfg(target_endian = "big")]
#[derive(Clone, Copy)]
#[repr(C)]
struct HeapRepr {
    _pad: [u8; PTR_PAD_LEN],
    ptr: *mut u8,
}

#[repr(C)]
pub(super) union Repr {
    inline: InlineRepr,
    heap: HeapRepr,
}

// The tag byte only aliases the least significant byte of the pointer as long as the union has
// exactly the expected size, refuse to compile rather than break silently if a target ever
// pads it differently.
//
// `assert!()` is not allowed in const context on MSRV, an out of bounds index fails the const
// evaluation instead.
const _: () = [(); 1][(size_of::<Repr>() != REPR_LEN) as usize];

impl Repr {
    /// Stores the first `len` bytes of `data` inline.
    ///
    /// # Panics
    ///
    /// Panics if `len` exceeds [`INLINE_LEN`].
    pub(super) const fn new_inline(data: [u8; INLINE_LEN], len: usize) -> Self {
        // `assert!()` is not allowed in const functions on MSRV, an out of bounds index panics
        // in both const and runtime context instead.
        let _ = ["hrp is too long to be stored inline"][(len > INLINE_LEN) as usize];

        // The set least significant bit is what distinguishes inline storage from a pointer to a
        // heap allocation.
        let tag = ((len as u8) << 1) | 1;

        #[cfg(target_endian = "little")]
        let inline = InlineRepr { tag, data, _pad: [0; PAD_LEN] };
        #[cfg(target_endian = "big")]
        let inline = InlineRepr { data, _pad: [0; PAD_LEN], tag };

        Self { inline }
    }

    /// Stores a pointer to a heap allocation that is aligned to [`HEAP_ALIGN`] bytes.
    #[cfg(feature = "alloc")]
    pub(super) fn new_heap(alloc_ptr: *mut u8) -> Self {
        let heap = HeapRepr { ptr: alloc_ptr, _pad: [0; PTR_PAD_LEN] };

        Self { heap }
    }

    /// Returns `true` if the bytes are stored inline rather than on the heap.
    pub(super) fn is_inline(&self) -> bool {
        // SAFETY: the tag byte is valid to read for both variants, for the heap variant it
        // aliases the least significant byte of the pointer, which is even due to the alignment
        // of the allocation
        let tag = unsafe { self.inline.tag };

        tag & 1 == 1
    }

    /// Copies the inline storage into a new instance.
    ///
    /// # Safety
    ///
    /// The bytes must be stored inline, as indicated by [`Self::is_inline()`].
    pub(super) unsafe fn copy_inline(&self) -> Self { Self { inline: self.inline } }

    /// Returns the bytes stored inline.
    ///
    /// # Safety
    ///
    /// The bytes must be stored inline, as indicated by [`Self::is_inline()`].
    pub(super) unsafe fn inline_bytes(&self) -> &[u8] {
        let len = usize::from(self.inline.tag >> 1);

        &self.inline.data[..len]
    }

    /// Same as [`Self::inline_bytes()`], but mutable.
    ///
    /// # Safety
    ///
    /// The bytes must be stored inline, as indicated by [`Self::is_inline()`].
    pub(super) unsafe fn inline_bytes_mut(&mut self) -> &mut [u8] {
        let len = usize::from(self.inline.tag >> 1);

        &mut self.inline.data[..len]
    }

    /// Returns the pointer to the heap allocation, pointing at the length prefix that is followed
    /// by that many bytes of data.
    ///
    /// # Safety
    ///
    /// The bytes must be stored on the heap, as indicated by [`Self::is_inline()`].
    pub(super) unsafe fn heap_alloc_ptr(&self) -> *mut u8 { self.heap.ptr }
}
