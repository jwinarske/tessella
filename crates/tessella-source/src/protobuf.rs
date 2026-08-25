//! Just enough protobuf to read a vector tile.
//!
//! # Why not a protobuf crate
//!
//! The vector-tile schema is four messages and a dozen fields, and it is frozen — MVT 2.1 is
//! the spec and there is no 3. A code generator would bring a build step, a dependency, and a
//! generated file to keep in step with a `.proto` that never changes, to save writing the
//! hundred lines below. DR-12 tracks binary size per target and DR-17 pins the toolchain; both
//! argue the same way.
//!
//! What is implemented is the wire format, not protobuf: varints, the four wire types, and
//! length-delimited fields. Anything a real library gives you beyond that — reflection,
//! descriptors, `Any`, groups — a vector tile never contains.
//!
//! # Unknown fields are skipped, not refused
//!
//! That is protobuf's rule and it is what makes the format extensible: a decoder must step over
//! a field it does not recognize rather than fail. Several of the spec's own "invalid" fixtures
//! are invalid *as vector tiles* while being perfectly well-formed protobuf, and a reader that
//! conflated the two would reject tiles a newer writer is entitled to produce.

use alloc::vec::Vec;

/// How a field's value is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireType {
    /// Base-128 varint.
    Varint,
    /// Fixed eight bytes.
    Fixed64,
    /// A length, then that many bytes.
    Delimited,
    /// Fixed four bytes.
    Fixed32,
}

/// A malformed protobuf stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    /// The buffer ended inside a value.
    #[error("truncated at byte {offset}")]
    Truncated {
        /// Where the read ran out.
        offset: usize,
    },
    /// A varint ran past ten bytes, which cannot encode a `u64`.
    #[error("varint at byte {offset} is longer than 64 bits")]
    VarintTooLong {
        /// Where the varint started.
        offset: usize,
    },
    /// Wire type 3 or 4: the deprecated group encoding.
    ///
    /// Refused rather than skipped, because skipping one means finding its matching end marker,
    /// and a vector tile has no groups to justify carrying that.
    #[error("wire type {wire} at byte {offset} is a group, which vector tiles do not use")]
    Group {
        /// The wire type read.
        wire: u8,
        /// Where the tag was.
        offset: usize,
    },
}

/// Reads protobuf fields out of a byte slice.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    /// A reader over a message's bytes.
    #[must_use]
    pub const fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    /// True when every byte has been read.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.position >= self.data.len()
    }

    /// How far in the reader is, for error messages.
    #[must_use]
    pub const fn position(&self) -> usize {
        self.position
    }

    /// The next field's number and wire type, or `None` at the end of the message.
    ///
    /// # Errors
    ///
    /// [`WireError`] when the tag is truncated or names a group.
    pub fn next_field(&mut self) -> Option<Result<(u32, WireType), WireError>> {
        if self.is_empty() {
            return None;
        }
        let offset = self.position;
        let tag = match self.varint() {
            Ok(tag) => tag,
            Err(err) => return Some(Err(err)),
        };
        #[allow(clippy::cast_possible_truncation)]
        let wire = (tag & 0x7) as u8;
        #[allow(clippy::cast_possible_truncation)]
        let number = (tag >> 3) as u32;
        let wire = match wire {
            0 => WireType::Varint,
            1 => WireType::Fixed64,
            2 => WireType::Delimited,
            5 => WireType::Fixed32,
            other => {
                return Some(Err(WireError::Group {
                    wire: other,
                    offset,
                }));
            }
        };
        Some(Ok((number, wire)))
    }

    /// A base-128 varint.
    ///
    /// # Errors
    ///
    /// [`WireError`] when it is truncated or longer than ten bytes.
    pub fn varint(&mut self) -> Result<u64, WireError> {
        let offset = self.position;
        let mut value: u64 = 0;
        for shift in 0..10 {
            let byte = *self
                .data
                .get(self.position)
                .ok_or(WireError::Truncated { offset })?;
            self.position += 1;
            value |= u64::from(byte & 0x7f) << (shift * 7);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(WireError::VarintTooLong { offset })
    }

    /// A length-delimited field's bytes.
    ///
    /// # Errors
    ///
    /// [`WireError::Truncated`] when the length runs past the end.
    pub fn delimited(&mut self) -> Result<&'a [u8], WireError> {
        let offset = self.position;
        let length =
            usize::try_from(self.varint()?).map_err(|_| WireError::Truncated { offset })?;
        let end = self
            .position
            .checked_add(length)
            .filter(|end| *end <= self.data.len())
            .ok_or(WireError::Truncated { offset })?;
        let bytes = &self.data[self.position..end];
        self.position = end;
        Ok(bytes)
    }

    /// Four bytes, little-endian.
    ///
    /// # Errors
    ///
    /// [`WireError::Truncated`] when fewer than four remain.
    pub fn fixed32(&mut self) -> Result<u32, WireError> {
        let offset = self.position;
        let end = self.position + 4;
        let bytes: [u8; 4] = self
            .data
            .get(self.position..end)
            .and_then(|slice| slice.try_into().ok())
            .ok_or(WireError::Truncated { offset })?;
        self.position = end;
        Ok(u32::from_le_bytes(bytes))
    }

    /// Eight bytes, little-endian.
    ///
    /// # Errors
    ///
    /// [`WireError::Truncated`] when fewer than eight remain.
    pub fn fixed64(&mut self) -> Result<u64, WireError> {
        let offset = self.position;
        let end = self.position + 8;
        let bytes: [u8; 8] = self
            .data
            .get(self.position..end)
            .and_then(|slice| slice.try_into().ok())
            .ok_or(WireError::Truncated { offset })?;
        self.position = end;
        Ok(u64::from_le_bytes(bytes))
    }

    /// Steps over a field of the given wire type.
    ///
    /// # Errors
    ///
    /// [`WireError`] when the field is truncated.
    pub fn skip(&mut self, wire: WireType) -> Result<(), WireError> {
        match wire {
            WireType::Varint => {
                self.varint()?;
            }
            WireType::Fixed64 => {
                self.fixed64()?;
            }
            WireType::Delimited => {
                self.delimited()?;
            }
            WireType::Fixed32 => {
                self.fixed32()?;
            }
        }
        Ok(())
    }

    /// Reads a packed repeated varint field, which is how tags and geometry travel.
    ///
    /// Accepts the unpacked form too — a field repeated one value at a time — because proto2
    /// writers are entitled to emit it and some tiles in the wild do.
    ///
    /// # Errors
    ///
    /// [`WireError`] when the field is truncated.
    pub fn packed_varints(&mut self, wire: WireType, out: &mut Vec<u32>) -> Result<(), WireError> {
        match wire {
            WireType::Delimited => {
                let bytes = self.delimited()?;
                // A varint is at least one byte, so the run holds at most this many. The buffer
                // is reused across a layer's features, so the reservation happens once and the
                // per-element capacity check afterwards is against a bound already met.
                out.reserve(bytes.len());

                let mut at = 0usize;
                while at < bytes.len() {
                    let first = bytes[at];
                    // The overwhelmingly common case, and the reason this is not a call to
                    // `varint`. Geometry deltas are zigzagged small numbers and tag entries are
                    // table indices, so nearly every varint in a tile is one byte — which the
                    // general decoder still reaches through a ten-iteration loop with a bounds
                    // check and an `Option` per byte. Profiled before this, `varint` and
                    // `packed_varints` were 39 % of the instructions a decode executes.
                    if first < 0x80 {
                        out.push(u32::from(first));
                        at += 1;
                        continue;
                    }

                    let offset = at;
                    let mut value: u64 = u64::from(first & 0x7f);
                    let mut shift = 7u32;
                    at += 1;
                    loop {
                        if shift >= 70 {
                            return Err(WireError::VarintTooLong { offset });
                        }
                        let byte = *bytes.get(at).ok_or(WireError::Truncated { offset })?;
                        at += 1;
                        value |= u64::from(byte & 0x7f) << shift;
                        if byte & 0x80 == 0 {
                            break;
                        }
                        shift += 7;
                    }
                    #[allow(clippy::cast_possible_truncation)]
                    out.push(value as u32);
                }
                Ok(())
            }
            WireType::Varint => {
                #[allow(clippy::cast_possible_truncation)]
                out.push(self.varint()? as u32);
                Ok(())
            }
            other => {
                self.skip(other)?;
                Ok(())
            }
        }
    }
}

/// Undoes protobuf's zigzag encoding, which is how signed geometry deltas travel.
///
/// Zigzag maps small negatives onto small unsigneds, so a delta of -1 costs one byte rather
/// than ten. Vector tile geometry is almost entirely small deltas, which is why the format uses
/// it and why decoding it wrongly produces coordinates that are enormous rather than merely
/// misplaced.
#[must_use]
pub const fn zigzag(value: u32) -> i32 {
    #[allow(clippy::cast_possible_wrap)]
    {
        ((value >> 1) as i32) ^ -((value & 1) as i32)
    }
}
