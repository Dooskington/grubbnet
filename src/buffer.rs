pub const MAX_BUFFER_SIZE: usize = 1024 * 16;

// TODO (Declan, 10/12/2019)
// We should probably be using a ring buffer instead.

/// A simple byte buffer, useful for storing bytes that are going to be consumed in packets.
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

    /// Deletes `count` bytes from the front of the buffer, then shifts the rest of the buffer back in place at index 0.
    pub fn drain(&mut self, count: usize) {
        unsafe {
            use std::ptr;
            ptr::copy(
                self.data.as_ptr().offset(count as isize),
                self.data.as_mut_ptr(),
                self.offset - count,
            );
        }

        self.offset -= count;
    }

    pub fn clear(&mut self) {
        self.data = [0; MAX_BUFFER_SIZE];
        self.offset = 0;
    }
}
