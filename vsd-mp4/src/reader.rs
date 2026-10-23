use std::io::{Cursor, Error, ErrorKind, Read, Result};

enum Endianness {
    Big,
    Little,
}

/// A reader for parsing binary data of MP4 containers with support for big-endian and little-endian formats.
pub struct Reader {
    endian: Endianness,
    inner: Cursor<Vec<u8>>,
}

impl Reader {
    /// Creates a new big-endian `Reader` for the given data.
    pub fn new_big_endian(data: &[u8]) -> Self {
        Self {
            endian: Endianness::Big,
            inner: Cursor::new(data.to_vec()),
        }
    }

    /// Creates a new little-endian `Reader` for the given data.
    pub fn new_little_endian(data: &[u8]) -> Self {
        Self {
            endian: Endianness::Little,
            inner: Cursor::new(data.to_vec()),
        }
    }

    /// Returns a slice referencing the underlying data.
    pub fn as_bytes(&self) -> &[u8] {
        self.inner.get_ref()
    }

    /// Returns `true` if there is more data to be read.
    pub fn has_more_data(&self) -> bool {
        self.inner.position() < (self.inner.get_ref().len() as u64)
    }

    /// Returns the total length of the data in bytes.
    pub fn get_length(&self) -> u64 {
        self.inner.get_ref().len() as u64
    }

    /// Returns the current read position.
    pub fn get_position(&self) -> u64 {
        self.inner.position()
    }

    /// Skips the specified number of bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if the new position exceeds the total data length.
    pub fn skip(&mut self, bytes: u64) -> Result<()> {
        let position = self.get_position().checked_add(bytes).ok_or_else(|| {
            Error::new(ErrorKind::OutOfMemory, "skip length overflows reader position.")
        })?;

        if position > self.get_length() {
            return Err(Error::new(
                ErrorKind::OutOfMemory,
                "skips out of memory bounds.",
            ));
        }

        self.inner.set_position(position);
        Ok(())
    }

    /// Reads a 8-bit unsigned integer.
    ///
    /// # Errors
    ///
    /// Returns an error if there is not enough data left to read.
    pub fn read_u8(&mut self) -> Result<u8> {
        let mut buf = [0; 1];
        self.inner.read_exact(&mut buf)?;
        Ok(buf[0])
    }

    /// Reads a 16-bit unsigned integer according to the configured endianness.
    ///
    /// # Errors
    ///
    /// Returns an error if there is not enough data left to read.
    pub fn read_u16(&mut self) -> Result<u16> {
        let mut buf = [0; 2];
        self.inner.read_exact(&mut buf)?;

        match self.endian {
            Endianness::Big => Ok(u16::from_be_bytes(buf)),
            Endianness::Little => Ok(u16::from_le_bytes(buf)),
        }
    }

    /// Reads a 32-bit unsigned integer according to the configured endianness.
    ///
    /// # Errors
    ///
    /// Returns an error if there is not enough data left to read.
    pub fn read_u32(&mut self) -> Result<u32> {
        let mut buf = [0; 4];
        self.inner.read_exact(&mut buf)?;

        match self.endian {
            Endianness::Big => Ok(u32::from_be_bytes(buf)),
            Endianness::Little => Ok(u32::from_le_bytes(buf)),
        }
    }

    /// Reads a 64-bit unsigned integer according to the configured endianness.
    ///
    /// # Errors
    ///
    /// Returns an error if there is not enough data left to read.
    pub fn read_u64(&mut self) -> Result<u64> {
        let mut buf = [0; 8];
        self.inner.read_exact(&mut buf)?;

        match self.endian {
            Endianness::Big => Ok(u64::from_be_bytes(buf)),
            Endianness::Little => Ok(u64::from_le_bytes(buf)),
        }
    }

    /// Reads the specified number of bytes into a vector.
    ///
    /// # Errors
    ///
    /// Returns an error if there is not enough data left to read.
    pub fn read_bytes_u8(&mut self, bytes: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0; bytes];
        self.inner.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Reads 16-bit unsigned integers from the configured number of bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if there is not enough data left to read.
    pub fn read_bytes_u16(&mut self, bytes: usize) -> Result<Vec<u16>> {
        Ok(self
            .read_bytes_u8(bytes)?
            .chunks_exact(2)
            .map(|x| match self.endian {
                Endianness::Big => u16::from_be_bytes([x[0], x[1]]),
                Endianness::Little => u16::from_le_bytes([x[0], x[1]]),
            })
            .collect())
    }

    /// Reads a 32-bit signed integer according to the configured endianness.
    ///
    /// # Errors
    ///
    /// Returns an error if there is not enough data left to read.
    pub fn read_i32(&mut self) -> Result<i32> {
        let mut buf = [0; 4];
        self.inner.read_exact(&mut buf)?;

        match self.endian {
            Endianness::Big => Ok(i32::from_be_bytes(buf)),
            Endianness::Little => Ok(i32::from_le_bytes(buf)),
        }
    }
}
