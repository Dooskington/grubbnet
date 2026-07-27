use crate::error::{Error, Result};

pub const MAX_BUFFER_SIZE: usize = 1024 * 16;

// TODO (Declan, 10/12/2019)
// We should probably be using a ring buffer instead.

/// A simple byte buffer, useful for storing bytes that are going to be consumed in packets.
///
/// # Invariant
///
/// `offset` must never exceed [`MAX_BUFFER_SIZE`]. Because `data` and `offset` are public
/// fields, that invariant cannot be enforced by construction, so every method that derives a
/// length or an index from `offset` re-establishes it first (see [`NetworkBuffer::len`]).
/// No method on this type can read or write out of bounds, whatever `offset` is set to.
pub struct NetworkBuffer {
    pub data: [u8; MAX_BUFFER_SIZE],
    pub offset: usize,
}

impl NetworkBuffer {
    pub fn new() -> Self {
        NetworkBuffer {
            data: [0; MAX_BUFFER_SIZE],
            offset: 0,
        }
    }

    /// The number of bytes currently buffered.
    ///
    /// This is `offset` clamped to [`MAX_BUFFER_SIZE`]. Always prefer this over reading
    /// `offset` directly: `offset` is a public field and may hold any value, so it must never
    /// be used to derive a length or an index without being clamped first.
    pub fn len(&self) -> usize {
        self.offset.min(MAX_BUFFER_SIZE)
    }

    /// Returns true if there are no bytes buffered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The number of bytes that can still be written into the buffer.
    pub fn remaining(&self) -> usize {
        MAX_BUFFER_SIZE - self.len()
    }

    /// Returns true if there is no room left to read more bytes into the buffer.
    pub fn is_full(&self) -> bool {
        self.remaining() == 0
    }

    /// The bytes currently buffered.
    pub fn filled(&self) -> &[u8] {
        let len = self.len();
        &self.data[..len]
    }

    /// The unwritten portion of the buffer, ready to be read into.
    ///
    /// This is recomputed from the current [`NetworkBuffer::len`] on every call, so it must be
    /// re-acquired after every write. Hoisting it out of a read loop is what caused the
    /// unbounded-`offset` heap corruption fixed in 0.2.3.
    pub fn writable(&mut self) -> &mut [u8] {
        let len = self.len();
        &mut self.data[len..]
    }

    /// Re-establishes the type invariant by clamping `offset` to [`MAX_BUFFER_SIZE`].
    ///
    /// `data` and `offset` are public fields, so any code - including entirely safe downstream
    /// code - can put the buffer into a state this crate would never produce. Call this before
    /// deriving any length or index from `offset`.
    pub fn normalize(&mut self) {
        self.offset = self.len();
    }

    /// Records that `count` bytes were written into the slice returned by
    /// [`NetworkBuffer::writable`].
    ///
    /// The resulting offset is clamped to [`MAX_BUFFER_SIZE`], so even a misbehaving `Read`
    /// implementation that reports more bytes than the destination slice could hold cannot
    /// push the buffer past the end of its own backing array.
    pub fn advance(&mut self, count: usize) {
        self.offset = self.offset.saturating_add(count).min(MAX_BUFFER_SIZE);
    }

    /// Deletes `count` bytes from the front of the buffer, then shifts the rest of the buffer
    /// back in place at index 0.
    ///
    /// Returns `Error::InvalidData` if `count` is greater than the number of buffered bytes,
    /// rather than panicking. Prefer this over [`NetworkBuffer::drain`] anywhere `count` is
    /// derived from network input: a panic in a single-process server is still a remote kill.
    pub fn try_drain(&mut self, count: usize) -> Result<()> {
        // `offset` is public, so re-establish the type invariant before deriving any length
        // from it. Everything below this line works off `len`, never off `self.offset`.
        self.normalize();
        let len = self.len();

        if count > len {
            return Err(Error::InvalidData);
        }

        // Bounds-checked equivalent of the `ptr::copy` this used to do inside an `unsafe`
        // block. `count <= len <= MAX_BUFFER_SIZE`, so the source range is always valid.
        self.data.copy_within(count..len, 0);
        self.offset = len - count;

        Ok(())
    }

    /// Deletes `count` bytes from the front of the buffer, then shifts the rest of the buffer back in place at index 0.
    ///
    /// # Panics
    ///
    /// Panics if `count` is greater than the number of buffered bytes. Kept for API
    /// compatibility; [`NetworkBuffer::try_drain`] is the preferred form, and is what this
    /// crate uses internally so that malformed input cannot take the process down.
    pub fn drain(&mut self, count: usize) {
        let len = self.len();
        if self.try_drain(count).is_err() {
            panic!(
                "Attempted to drain more bytes ({}) than present in buffer ({})",
                count, len
            );
        }
    }

    pub fn clear(&mut self) {
        self.data = [0; MAX_BUFFER_SIZE];
        self.offset = 0;
    }
}

impl Default for NetworkBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a buffer holding `bytes`, with a correct offset.
    fn buffer_with(bytes: &[u8]) -> NetworkBuffer {
        let mut buffer = NetworkBuffer::new();
        buffer.data[..bytes.len()].copy_from_slice(bytes);
        buffer.offset = bytes.len();
        buffer
    }

    #[test]
    fn drain_zero_is_a_no_op() {
        let mut buffer = buffer_with(&[1, 2, 3, 4]);

        buffer.drain(0);

        assert_eq!(buffer.offset, 4);
        assert_eq!(buffer.filled(), &[1, 2, 3, 4]);
    }

    #[test]
    fn drain_some_shifts_the_remainder_to_the_front() {
        let mut buffer = buffer_with(&[1, 2, 3, 4, 5]);

        buffer.drain(2);

        assert_eq!(buffer.offset, 3);
        assert_eq!(buffer.filled(), &[3, 4, 5]);
    }

    #[test]
    fn drain_everything_empties_the_buffer() {
        let mut buffer = buffer_with(&[1, 2, 3, 4]);

        buffer.drain(4);

        assert_eq!(buffer.offset, 0);
        assert!(buffer.is_empty());
        assert_eq!(buffer.filled(), &[] as &[u8]);
    }

    #[test]
    fn drain_one_from_a_completely_full_buffer_stays_in_bounds() {
        let mut buffer = NetworkBuffer::new();
        buffer.offset = MAX_BUFFER_SIZE;
        buffer.data[MAX_BUFFER_SIZE - 1] = 0xAB;

        buffer.try_drain(1).expect("draining a full buffer failed");

        assert_eq!(buffer.offset, MAX_BUFFER_SIZE - 1);
        assert_eq!(buffer.data[MAX_BUFFER_SIZE - 2], 0xAB);
    }

    #[test]
    fn try_drain_more_than_present_is_an_error() {
        let mut buffer = buffer_with(&[1, 2, 3]);

        assert!(matches!(buffer.try_drain(4), Err(Error::InvalidData)));

        // The buffer is left untouched, so the caller can decide what to do about it.
        assert_eq!(buffer.offset, 3);
        assert_eq!(buffer.filled(), &[1, 2, 3]);
    }

    #[test]
    #[should_panic(expected = "Attempted to drain more bytes")]
    fn drain_more_than_present_panics() {
        let mut buffer = buffer_with(&[1, 2, 3]);
        buffer.drain(4);
    }

    /// The soundness regression test for the original bug. `offset` is a public field, so
    /// entirely safe downstream code could set it past the end of `data`. `drain` then derived
    /// a raw copy length from it and handed that to `ptr::copy`, reading and writing tens of
    /// kilobytes past the end of a heap-resident array.
    #[test]
    fn offset_beyond_capacity_never_reads_or_writes_out_of_bounds() {
        let mut buffer = NetworkBuffer::new();
        buffer.data[..4].copy_from_slice(&[1, 2, 3, 4]);

        // The exact offset the vulnerable read loop produced from a single 64 KB burst.
        buffer.offset = 65536;

        // In 0.2.2 this was `ptr::copy(data + 3, data, 65533)` over a 16384-byte array.
        buffer
            .try_drain(3)
            .expect("drain of a clamped buffer failed");

        // The offset is clamped to capacity first, then the drain applies to the clamped value.
        assert_eq!(buffer.offset, MAX_BUFFER_SIZE - 3);
        assert_eq!(buffer.data[0], 4);
    }

    #[test]
    fn absurd_offset_is_clamped_rather_than_overflowing() {
        let mut buffer = NetworkBuffer::new();

        // The documented safe-code path to UB in 0.2.2:
        //     let mut b = grubbnet::buffer::NetworkBuffer::new();
        //     b.offset = usize::MAX;
        //     b.drain(0);
        buffer.offset = usize::MAX;
        buffer
            .try_drain(0)
            .expect("drain of a clamped buffer failed");

        assert_eq!(buffer.offset, MAX_BUFFER_SIZE);
        assert_eq!(buffer.len(), MAX_BUFFER_SIZE);
        assert!(buffer.is_full());
    }

    #[test]
    fn absurd_offset_rejects_an_absurd_count() {
        let mut buffer = NetworkBuffer::new();
        buffer.offset = usize::MAX;

        assert!(matches!(
            buffer.try_drain(usize::MAX),
            Err(Error::InvalidData)
        ));
        assert_eq!(buffer.offset, MAX_BUFFER_SIZE);
    }

    #[test]
    fn accessors_clamp_a_hostile_offset() {
        let mut buffer = NetworkBuffer::new();
        buffer.offset = usize::MAX;

        assert_eq!(buffer.len(), MAX_BUFFER_SIZE);
        assert_eq!(buffer.remaining(), 0);
        assert!(buffer.is_full());
        assert_eq!(buffer.filled().len(), MAX_BUFFER_SIZE);
        assert_eq!(buffer.writable().len(), 0);
    }

    #[test]
    fn writable_shrinks_as_the_buffer_fills() {
        let mut buffer = NetworkBuffer::new();
        assert_eq!(buffer.writable().len(), MAX_BUFFER_SIZE);

        buffer.advance(100);

        assert_eq!(buffer.writable().len(), MAX_BUFFER_SIZE - 100);
        assert_eq!(buffer.remaining(), MAX_BUFFER_SIZE - 100);
        assert!(!buffer.is_full());
    }

    #[test]
    fn advance_saturates_at_capacity() {
        let mut buffer = NetworkBuffer::new();

        // A `Read` impl reporting more bytes than the slice it was handed must not be able to
        // push the offset past the end of the array.
        buffer.advance(usize::MAX);
        assert_eq!(buffer.offset, MAX_BUFFER_SIZE);

        buffer.advance(usize::MAX);
        assert_eq!(buffer.offset, MAX_BUFFER_SIZE);
    }

    #[test]
    fn repeated_full_size_advances_cannot_exceed_capacity() {
        // Mirrors the vulnerable read loop: four reads of MAX_BUFFER_SIZE bytes each.
        let mut buffer = NetworkBuffer::new();
        for _ in 0..4 {
            buffer.advance(MAX_BUFFER_SIZE);
            assert!(buffer.offset <= MAX_BUFFER_SIZE);
        }

        assert_eq!(buffer.offset, MAX_BUFFER_SIZE);
    }

    #[test]
    fn clear_resets_everything() {
        let mut buffer = buffer_with(&[1, 2, 3]);

        buffer.clear();

        assert_eq!(buffer.offset, 0);
        assert!(buffer.is_empty());
        assert_eq!(buffer.data[0], 0);
    }
}
