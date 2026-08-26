//! Reading a PMTiles v3 archive directly, without a server in front of it.
//!
//! # Why read the archive rather than serve it
//!
//! A PMTiles file is a whole tileset in one file: a header, a directory, and the tiles laid out
//! contiguously in Hilbert order so that tiles near each other on the map are near each other on
//! disk. Nothing about using one requires HTTP — `pmtiles serve` exists to put a tileset behind a
//! URL, and a map reading a local archive has no use for the URL.
//!
//! That matters for the shape this frontend is aimed at. An embedded target with a region on
//! local storage should read it, not run a web server against itself to fetch from localhost —
//! which costs a socket, a copy, and a process that can fail independently of the map.
//!
//! # The format, and the two things worth knowing about it
//!
//! Tiles are addressed by a single id rather than by `z/x/y`: a Hilbert curve index within the
//! zoom level, offset by every level below it. A Hilbert curve keeps neighbours adjacent, which
//! is what makes a viewport's worth of tiles a small number of contiguous reads rather than
//! nine scattered ones — and what lets the directory run-length encode them.
//!
//! Directories are varint arrays, and a directory entry with a run length of zero is not a tile.
//! It is a pointer to a *leaf* directory holding the entries for that range, which is how an
//! archive of a hundred million tiles keeps its root directory small enough to read at once.

/// Why an archive could not be read.
#[derive(Debug, thiserror::Error)]
pub enum PmtilesError {
    /// The file does not start with the PMTiles magic.
    #[error("not a PMTiles archive")]
    NotAnArchive,
    /// A version this does not implement.
    ///
    /// Refused rather than attempted: versions 1 and 2 have a different directory format
    /// entirely, and reading one as v3 would produce plausible offsets into the wrong bytes.
    #[error("PMTiles version {0} is not supported; this reads version 3")]
    UnsupportedVersion(u8),
    /// A compression this does not implement.
    #[error("compression {0} is not supported")]
    UnsupportedCompression(u8),
    /// The file ended where a structure was expected.
    #[error("truncated: {0}")]
    Truncated(&'static str),
    /// A directory did not decode.
    #[error("malformed directory: {0}")]
    Malformed(&'static str),
    /// The underlying reader failed.
    #[error("reading the archive: {0}")]
    Io(String),
    /// Decompressing a range would have produced more than [`MAX_RESOURCE_BYTES`] bytes.
    ///
    /// Refused rather than truncated. A truncated tile decodes as a protobuf wire error several
    /// steps away from the cause, which is the failure this module's own notes argue against —
    /// and a caller that saw a short tile could not tell a bomb from a corrupt archive.
    ///
    /// [`MAX_RESOURCE_BYTES`]: crate::source::MAX_RESOURCE_BYTES
    #[error("a range decompressed past the {limit}-byte ceiling")]
    TooLarge {
        /// The ceiling that was exceeded.
        limit: u64,
    },
}

/// How a byte range is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// The archive does not say.
    Unknown,
    /// Stored as-is.
    None,
    /// gzip.
    Gzip,
    /// brotli, which this does not implement.
    Brotli,
    /// zstd, which this does not implement.
    Zstd,
}

impl Compression {
    fn from_byte(byte: u8) -> Result<Self, PmtilesError> {
        match byte {
            0 => Ok(Self::Unknown),
            1 => Ok(Self::None),
            2 => Ok(Self::Gzip),
            3 => Ok(Self::Brotli),
            4 => Ok(Self::Zstd),
            other => Err(PmtilesError::UnsupportedCompression(other)),
        }
    }
}

/// What the tiles in an archive are.
///
/// Worth reading rather than assuming: an archive of PNG rasters and one of vector tiles are
/// the same container, and handing raster bytes to the MVT decoder produces a parse error
/// several layers away from the thing that was actually wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileType {
    /// The archive does not say.
    Unknown,
    /// Mapbox Vector Tile.
    Mvt,
    /// PNG.
    Png,
    /// JPEG.
    Jpeg,
    /// WebP.
    Webp,
    /// AVIF.
    Avif,
    /// MapLibre Tile, which nothing here decodes yet.
    Mlt,
    /// A type postdating this reader. Carried rather than refused: the directory format does
    /// not depend on it, so an unknown type is only a problem for whoever decodes the bytes.
    Other(u8),
}

impl TileType {
    const fn from_byte(byte: u8) -> Self {
        match byte {
            0 => Self::Unknown,
            1 => Self::Mvt,
            2 => Self::Png,
            3 => Self::Jpeg,
            4 => Self::Webp,
            5 => Self::Avif,
            6 => Self::Mlt,
            other => Self::Other(other),
        }
    }
}

/// The 127-byte header at the front of every v3 archive.
///
/// Not `Eq`: the geographic fields are floats. They are exact — every one is an `i32` divided
/// by a power of ten — but a type is not `Eq` because its values happen to be.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Header {
    /// Where the root directory starts, and how long it is.
    pub root: (u64, u64),
    /// Where the metadata document starts, and how long it is.
    pub metadata: (u64, u64),
    /// Where leaf directories start, and how long that region is.
    pub leaves: (u64, u64),
    /// Where tile data starts, and how long that region is.
    pub tiles: (u64, u64),
    /// How the directories and metadata are compressed.
    pub internal_compression: Compression,
    /// How the tiles themselves are compressed.
    pub tile_compression: Compression,
    /// What the tiles are.
    pub tile_type: TileType,
    /// Lowest zoom the archive holds.
    pub min_zoom: u8,
    /// Highest zoom the archive holds.
    pub max_zoom: u8,
    /// West, south, east, north, in degrees.
    pub bounds: [f64; 4],
    /// Longitude and latitude the archive suggests opening at, in degrees.
    pub center: [f64; 2],
    /// Zoom the archive suggests opening at.
    pub center_zoom: u8,
}

impl Header {
    /// The header's fixed length.
    pub const LEN: usize = 127;

    /// Parses the header from the first [`Self::LEN`] bytes of an archive.
    ///
    /// # Errors
    ///
    /// [`PmtilesError::NotAnArchive`] when the magic is wrong,
    /// [`PmtilesError::UnsupportedVersion`] for anything but version 3, and
    /// [`PmtilesError::Truncated`] when there are not enough bytes.
    pub fn parse(bytes: &[u8]) -> Result<Self, PmtilesError> {
        if bytes.len() < Self::LEN {
            return Err(PmtilesError::Truncated("header"));
        }
        if &bytes[..7] != b"PMTiles" {
            return Err(PmtilesError::NotAnArchive);
        }
        if bytes[7] != 3 {
            return Err(PmtilesError::UnsupportedVersion(bytes[7]));
        }
        let at = |offset: usize| {
            let mut eight = [0u8; 8];
            eight.copy_from_slice(&bytes[offset..offset + 8]);
            u64::from_le_bytes(eight)
        };
        // The geographic fields are stored as degrees times ten million, signed, which is what
        // keeps them exact in four bytes and what makes reading them as unsigned put every
        // western longitude on the far side of the world.
        let degrees = |offset: usize| {
            let mut four = [0u8; 4];
            four.copy_from_slice(&bytes[offset..offset + 4]);
            f64::from(i32::from_le_bytes(four)) / 1e7
        };
        Ok(Self {
            root: (at(8), at(16)),
            metadata: (at(24), at(32)),
            leaves: (at(40), at(48)),
            tiles: (at(56), at(64)),
            internal_compression: Compression::from_byte(bytes[97])?,
            tile_compression: Compression::from_byte(bytes[98])?,
            tile_type: TileType::from_byte(bytes[99]),
            min_zoom: bytes[100],
            max_zoom: bytes[101],
            bounds: [degrees(102), degrees(106), degrees(110), degrees(114)],
            center: [degrees(119), degrees(123)],
            center_zoom: bytes[118],
        })
    }
}

/// The archive-wide id of a tile.
///
/// A Hilbert curve index within the zoom level, offset past every level below it. Transcribed
/// from the PMTiles reference implementation rather than derived: the curve has several
/// conventions that differ in which corner they start from, and picking the wrong one produces
/// ids that are plausible, contiguous, and address the wrong tiles.
///
/// `None` when the coordinate is not one: `x` or `y` at or past `2^z`, or a zoom past 31, where
/// the level's id range no longer fits a `u64`.
///
/// # Why this is an `Option` and not a precondition
///
/// The walk masks `x` and `y` one bit at a time against `s < 2^z`, so a coordinate with a bit
/// above that range simply loses it: `z1/x2/y0` walks identically to `z1/x0/y0` and returns its
/// id. Not an error, not an empty result — *a different tile's bytes*, which is the failure
/// that gets drawn rather than reported. Once a tile URL is parsed from a string, `x` is
/// whatever the string said, so the range check has to live where the id is computed. The zoom
/// bound is the more ordinary hazard: `1 << (2 * z)` is a shift past the width for `z > 31`,
/// which panics in a debug build and wraps in a release one.
#[must_use]
pub fn tile_id(z: u8, x: u32, y: u32) -> Option<u64> {
    if z > 31 || u64::from(x) >= 1u64 << u32::from(z) || u64::from(y) >= 1u64 << u32::from(z) {
        return None;
    }
    // Every level below this one, which is `sum(4^i)` for `i < z`.
    let base = ((1u64 << (2 * u32::from(z))) - 1) / 3;

    let (mut tx, mut ty) = (u64::from(x), u64::from(y));
    let mut d = 0u64;
    let mut s = 1u64 << u32::from(z) >> 1;
    while s > 0 {
        let rx = u64::from(tx & s > 0);
        let ry = u64::from(ty & s > 0);
        d += s * s * ((3 * rx) ^ ry);
        if ry == 0 {
            if rx == 1 {
                tx = s.wrapping_sub(1).wrapping_sub(tx);
                ty = s.wrapping_sub(1).wrapping_sub(ty);
            }
            core::mem::swap(&mut tx, &mut ty);
        }
        s >>= 1;
    }
    Some(base + d)
}

/// One directory entry: a run of tiles, or a pointer to a leaf directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    id: u64,
    offset: u64,
    length: u32,
    /// How many consecutive ids share this byte range. Zero means a leaf-directory pointer.
    run: u32,
}

/// Reads a varint from `bytes` at `at`, advancing it.
fn varint(bytes: &[u8], at: &mut usize) -> Result<u64, PmtilesError> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *bytes
            .get(*at)
            .ok_or(PmtilesError::Malformed("varint ran off the end"))?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift >= 64 {
            return Err(PmtilesError::Malformed("varint too long"));
        }
    }
}

/// Decodes a directory.
///
/// The layout is column-major: every id, then every run length, then every length, then every
/// offset. Ids are delta-encoded, and an offset of zero means "immediately after the previous
/// entry" — which is what makes a clustered archive's directory mostly zeroes and so mostly
/// one byte per entry.
fn directory(bytes: &[u8]) -> Result<Vec<Entry>, PmtilesError> {
    let mut at = 0usize;
    let count = usize::try_from(varint(bytes, &mut at)?)
        .map_err(|_| PmtilesError::Malformed("entry count too large"))?;
    // A directory is bounded by its own bytes: every entry costs at least four varints of at
    // least one byte, so a count larger than that is a malformed length rather than a very
    // large directory, and allocating for it first would be the wrong order of operations.
    if count > bytes.len() {
        return Err(PmtilesError::Malformed("more entries than bytes"));
    }
    let mut entries = Vec::with_capacity(count);

    let mut id = 0u64;
    for _ in 0..count {
        id += varint(bytes, &mut at)?;
        entries.push(Entry {
            id,
            offset: 0,
            length: 0,
            run: 0,
        });
    }
    for entry in &mut entries {
        entry.run = u32::try_from(varint(bytes, &mut at)?)
            .map_err(|_| PmtilesError::Malformed("run length too large"))?;
    }
    for entry in &mut entries {
        entry.length = u32::try_from(varint(bytes, &mut at)?)
            .map_err(|_| PmtilesError::Malformed("entry length too large"))?;
    }
    for index in 0..entries.len() {
        let raw = varint(bytes, &mut at)?;
        entries[index].offset = if raw == 0 {
            let previous = index
                .checked_sub(1)
                .and_then(|before| entries.get(before))
                .ok_or(PmtilesError::Malformed("first entry has no predecessor"))?;
            previous.offset + u64::from(previous.length)
        } else {
            raw - 1
        };
    }
    Ok(entries)
}

/// The entry covering `id`, if the directory has one.
fn find(entries: &[Entry], id: u64) -> Option<&Entry> {
    // The largest entry whose id does not exceed the target: a run covers the ids after its
    // own, so the match is the one before the first that is too large.
    let at = entries
        .partition_point(|entry| entry.id <= id)
        .checked_sub(1)?;
    let entry = &entries[at];
    if entry.run == 0 {
        // A leaf pointer covers everything from its id onwards until the next entry, so the
        // caller has to follow it rather than compare against a run.
        return Some(entry);
    }
    (id < entry.id + u64::from(entry.run)).then_some(entry)
}

/// Somewhere an archive's bytes can be read from by range.
///
/// A trait rather than a `File` so the same reader serves a memory-mapped archive, a slice in a
/// test, and — when §12.6's range requests arrive — an HTTP origin, which is how mbgl reads a
/// remote `pmtiles://`. What every one of those has in common is answering "give me these bytes",
/// and nothing else about them is this module's business.
pub trait RangeReader {
    /// Reads exactly `length` bytes starting at `offset`.
    ///
    /// # Errors
    ///
    /// [`PmtilesError::Io`] when the range cannot be read, and
    /// [`PmtilesError::Truncated`] when it runs past the end.
    fn read_at(&self, offset: u64, length: usize) -> Result<Vec<u8>, PmtilesError>;
}

impl RangeReader for &[u8] {
    fn read_at(&self, offset: u64, length: usize) -> Result<Vec<u8>, PmtilesError> {
        let start = usize::try_from(offset).map_err(|_| PmtilesError::Truncated("range"))?;
        self.get(start..start + length)
            .map(<[u8]>::to_vec)
            .ok_or(PmtilesError::Truncated("range"))
    }
}

impl RangeReader for std::fs::File {
    fn read_at(&self, offset: u64, length: usize) -> Result<Vec<u8>, PmtilesError> {
        use std::os::unix::fs::FileExt;
        let mut out = vec![0u8; length];
        self.read_exact_at(&mut out, offset)
            .map_err(|error| PmtilesError::Io(format!("{error}")))?;
        Ok(out)
    }
}

/// Inflates a gzip range, for the bomb test.
///
/// The bound is on the private `decompress`, which is reached only through an archive — building
/// one whose *tile* expands past the ceiling means writing a valid header, a directory and an
/// offset, none of which is what the test is about. This is the same call with the compression
/// fixed, exposed so the ceiling can be exercised directly.
#[doc(hidden)]
pub fn inflate_for_test(bytes: Vec<u8>) -> Result<Vec<u8>, PmtilesError> {
    decompress(bytes, Compression::Gzip)
}

/// Inflates a range according to the archive's stated compression.
fn decompress(bytes: Vec<u8>, compression: Compression) -> Result<Vec<u8>, PmtilesError> {
    match compression {
        // Unknown means the archive declined to say, and every archive in the wild that does so
        // stores plainly. Guessing gzip and being wrong is a decode error much later.
        Compression::None | Compression::Unknown => Ok(bytes),
        Compression::Gzip => {
            use std::io::Read;
            let mut out = Vec::new();
            // Bounded. A gzip member a few hundred bytes long can expand without limit, and an
            // archive is a file from somewhere else however it arrived — `read_to_end` on an
            // untrusted stream is the allocation this crate must not make.
            let limit = crate::source::MAX_RESOURCE_BYTES;
            // One byte past the ceiling, so passing it is observable: `take` truncates
            // silently, and a short tile is indistinguishable from a corrupt one.
            flate2::read::GzDecoder::new(bytes.as_slice())
                .take(limit + 1)
                .read_to_end(&mut out)
                .map_err(|error| PmtilesError::Io(format!("gunzip: {error}")))?;
            if out.len() as u64 > limit {
                return Err(PmtilesError::TooLarge { limit });
            }
            Ok(out)
        }
        Compression::Brotli => Err(PmtilesError::UnsupportedCompression(3)),
        Compression::Zstd => Err(PmtilesError::UnsupportedCompression(4)),
    }
}

pub mod source;

/// A PMTiles archive, read in place.
#[derive(Debug)]
pub struct Archive<R> {
    source: R,
    header: Header,
    root: Vec<Entry>,
}

impl<R: RangeReader> Archive<R> {
    /// Reads the header and root directory.
    ///
    /// # Errors
    ///
    /// [`PmtilesError`] when the archive is not one, is a version this does not read, or is
    /// truncated.
    pub fn open(source: R) -> Result<Self, PmtilesError> {
        let header = Header::parse(&source.read_at(0, Header::LEN)?)?;
        let raw = source.read_at(
            header.root.0,
            usize::try_from(header.root.1).map_err(|_| PmtilesError::Truncated("root"))?,
        )?;
        let root = directory(&decompress(raw, header.internal_compression)?)?;
        Ok(Self {
            source,
            header,
            root,
        })
    }

    /// What the archive says about itself.
    #[must_use]
    pub const fn header(&self) -> &Header {
        &self.header
    }

    /// The archive's metadata document, decompressed.
    ///
    /// # Errors
    ///
    /// [`PmtilesError`] when the range cannot be read or decompressed.
    pub fn metadata(&self) -> Result<Vec<u8>, PmtilesError> {
        let raw = self.source.read_at(
            self.header.metadata.0,
            usize::try_from(self.header.metadata.1)
                .map_err(|_| PmtilesError::Truncated("metadata"))?,
        )?;
        decompress(raw, self.header.internal_compression)
    }

    /// One tile, decompressed, or `None` when the archive does not hold it.
    ///
    /// A missing tile is `None` rather than an error, for the same reason a 404 from an origin
    /// is a response: a tileset's coverage is not a rectangle, and asking for a tile outside it
    /// is how the edge is found.
    ///
    /// # Errors
    ///
    /// [`PmtilesError`] when a directory is malformed or a range cannot be read.
    pub fn tile(&self, z: u8, x: u32, y: u32) -> Result<Option<Vec<u8>>, PmtilesError> {
        if z < self.header.min_zoom || z > self.header.max_zoom {
            return Ok(None);
        }
        let Some(id) = tile_id(z, x, y) else {
            return Ok(None);
        };

        let mut entries = self.root.clone();
        // Leaf directories may nest. The reference implementation caps the walk at three levels
        // and so does this: a deeper chain is a malformed archive rather than a very large one,
        // and following it unbounded is how a crafted file turns into an unbounded read loop.
        for _ in 0..4 {
            let Some(entry) = find(&entries, id) else {
                return Ok(None);
            };
            if entry.run > 0 {
                let raw = self
                    .source
                    .read_at(self.header.tiles.0 + entry.offset, entry.length as usize)?;
                return decompress(raw, self.header.tile_compression).map(Some);
            }
            // A run of zero is a pointer to the directory that covers this range.
            //
            // Not cached, unlike mbgl, which holds a hundred directories. Measured on a Berlin
            // z15 archive: a z14 tile costs 210us end to end and 195us of that is inflating the
            // tile body, so the whole walk — this read, its gunzip, and both lookups — is under
            // 15us. A cache would buy under 5%, which is less than the spread between two
            // adjacent tiles. That arithmetic is a property of *local* reads: once §12.6's
            // range requests give this a remote reader, a leaf read becomes a round trip and
            // the cache stops being optional.
            let raw = self
                .source
                .read_at(self.header.leaves.0 + entry.offset, entry.length as usize)?;
            entries = directory(&decompress(raw, self.header.internal_compression)?)?;
        }
        Err(PmtilesError::Malformed(
            "leaf directories nested too deeply",
        ))
    }
}
