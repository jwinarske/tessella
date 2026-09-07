//! Watching one label across frames.
//!
//! # Why this exists
//!
//! Because the label fades have been switched on three times and made the picture worse each
//! time, and every attempt to find out why worked from *pictures*. A picture says the frame is
//! wrong; it does not say which label, or when it went wrong, or which of the several values
//! that decide a glyph's opacity disagreed. Diffing pixels found the first defect and then
//! misled twice, and what finally worked was dumping an intermediate and comparing it. This is
//! that, made permanent and aimed at the one quantity the fades move.
//!
//! # What it records
//!
//! One line per watched label per frame, carrying every value between the placement decision and
//! the byte the shader reads:
//!
//! - the frame, so a trajectory can be read down the column;
//! - the label's text and its cross-tile id, which is what a fade is keyed by — a label whose id
//!   changes between frames is a *different* label to the fades, and that alone would explain a
//!   fade that never arrives;
//! - whether the line walk found room for it this frame, since a label without room is hidden by
//!   `write_line_positions` and was, until recently, un-hidden again by `write_opacity`;
//! - the opacity the fade holds for that id;
//! - and the opacity actually written into the vertex, decoded back out of the packed value.
//!
//! The last two are the pair that matters. They should agree, and where they do not the
//! difference is between what placement decided and what the GPU was told.
//!
//! # Cost when it is off
//!
//! One relaxed load of an `Option`'s discriminant. The pattern is read from the environment once;
//! everything else is behind that check, and the whole module compiles out where the crate has no
//! `std` to read an environment from.

/// Whether anything is being watched, and what.
///
/// `TESSELLA_WATCH` is matched as a substring of the label's text, so `TESSELLA_WATCH=Madison`
/// follows every Madison Street on the map and `TESSELLA_WATCH=` follows all of them — which is
/// a great deal of output and occasionally what is wanted.
#[cfg(feature = "std")]
fn pattern() -> Option<&'static str> {
    use std::sync::OnceLock;
    static PATTERN: OnceLock<Option<alloc::string::String>> = OnceLock::new();
    PATTERN
        .get_or_init(|| std::env::var("TESSELLA_WATCH").ok())
        .as_deref()
}

/// The frame a line is stamped with.
#[cfg(feature = "std")]
fn counter() -> &'static core::sync::atomic::AtomicU64 {
    static FRAME: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    &FRAME
}

/// Starts a frame, so the lines below it can be told from the last frame's.
#[cfg(feature = "std")]
pub fn begin_frame() {
    if pattern().is_some() {
        counter().fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
}

/// Starts a frame. Compiled out without `std`.
#[cfg(not(feature = "std"))]
pub const fn begin_frame() {}

/// Whether anything at all is being watched, for a caller with work to do before it can report.
#[cfg(feature = "std")]
#[must_use]
pub fn watching() -> bool {
    pattern().is_some()
}

/// Whether anything is being watched. Always false without `std`.
#[cfg(not(feature = "std"))]
#[must_use]
pub const fn watching() -> bool {
    false
}

/// Whether this label is being followed.
#[cfg(feature = "std")]
#[must_use]
pub fn follows(text: &str) -> bool {
    pattern().is_some_and(|wanted| text.contains(wanted))
}

/// Whether this label is being followed. Always false without `std`.
#[cfg(not(feature = "std"))]
#[must_use]
pub const fn follows(_text: &str) -> bool {
    false
}

/// What one label looked like on one frame.
#[derive(Debug, Clone, Copy)]
pub struct Sighting {
    /// The identity the fade is keyed by.
    pub cross_tile_id: u32,
    /// Where the label is anchored, in tile units.
    pub anchor: (f32, f32),
    /// The tile it came from.
    pub tile: (u8, u32, u32),
    /// Whether the line walk found room for it this frame.
    pub has_room: bool,
    /// The opacity the fade holds, or `None` where it holds nothing for this id.
    pub fade: Option<f32>,
    /// The packed opacity vertex actually written, as the shader will read it.
    pub vertex: Option<f32>,
}

/// Records one label on one frame.
///
/// `vertex` is decoded rather than printed raw: `opacity_vertex` packs the opacity into the high
/// seven bits and whether it was placed into the low one, so the number in the buffer means
/// nothing until it is taken apart.
#[cfg(feature = "std")]
pub fn note(text: &str, seen: &Sighting) {
    if !follows(text) {
        return;
    }
    let frame = counter().load(core::sync::atomic::Ordering::Relaxed);
    let (placed, opacity) = match seen.vertex {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some(packed) => {
            let bits = packed as u8;
            (Some(bits & 1 == 1), Some(f32::from(bits >> 1) / 127.0))
        }
        None => (None, None),
    };
    std::println!(
        "watch frame={frame} text={text:?} id={id} tile={z}/{x}/{y} anchor={ax:.0},{ay:.0} \
         room={room} fade={fade} vertex_opacity={vo} vertex_placed={vp}",
        id = seen.cross_tile_id,
        z = seen.tile.0,
        x = seen.tile.1,
        y = seen.tile.2,
        ax = seen.anchor.0,
        ay = seen.anchor.1,
        room = seen.has_room,
        fade = seen
            .fade
            .map_or_else(|| "none".into(), |v| alloc::format!("{v:.3}")),
        vo = opacity.map_or_else(|| "none".into(), |v| alloc::format!("{v:.3}")),
        vp = placed.map_or_else(|| "none".into(), |v| alloc::format!("{v}")),
    );
}

/// Records one label on one frame. Compiled out without `std`.
#[cfg(not(feature = "std"))]
pub const fn note(_text: &str, _seen: &Sighting) {}

/// How a frame ended, so a label seen inside it can be told from one that was published.
///
/// `place_symbols` runs while the frame is being *built*. A frame that then fails is aborted --
/// `producer.abort()` leaves head where it was and the arena rewinds -- so every record it wrote,
/// including the geometry carrying the opacity just recorded, is discarded. Without this line the
/// two look identical from the producer's side: the watch says the label reached full opacity and
/// the consumer never receives it, with nothing to say the frame was thrown away.
#[cfg(feature = "std")]
pub fn frame_end(geometries: usize, published: bool) {
    if pattern().is_none() {
        return;
    }
    let frame = counter().load(core::sync::atomic::Ordering::Relaxed);
    std::println!("watch frame={frame} end geometries={geometries} published={published}");
}

/// How a frame ended. Compiled out without `std`.
#[cfg(not(feature = "std"))]
pub const fn frame_end(_geometries: usize, _published: bool) {}

/// The bytes a geometry record names, as it is written.
///
/// Paired with the consumer's `TSF_WATCH_FADE`, which prints what it resolves the same reference
/// to. Two shapes remain for a buffer that is published holding one value and received holding
/// another, and this is what tells them apart: either the record names different bytes than the
/// ones just written, or the bytes are overwritten between the publish and the read. The first
/// shows as a slab reference that disagrees with the opacity beside it; the second as references
/// that agree and contents that do not.
#[cfg(feature = "std")]
pub fn sent(
    view: u32,
    id: u64,
    attr_id: u32,
    slab: u32,
    offset: u32,
    length: u32,
    first_opacity: Option<f32>,
) {
    if pattern().is_none() {
        return;
    }
    let frame = counter().load(core::sync::atomic::Ordering::Relaxed);
    std::println!(
        "sent frame={frame} view={view} id={id} attr={attr_id} slab={slab} offset={offset} \
         length={length} opacity={opacity}",
        opacity = first_opacity.map_or_else(
            || alloc::string::String::from("none"),
            |v| alloc::format!("{v:.3}")
        ),
    );
}

/// The bytes a geometry record names. Compiled out without `std`.
#[cfg(not(feature = "std"))]
pub const fn sent(
    _view: u32,
    _id: u64,
    _attr_id: u32,
    _slab: u32,
    _offset: u32,
    _length: u32,
    _first_opacity: Option<f32>,
) {
}
