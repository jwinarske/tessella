//! A hillshade and a color relief are clipped like a raster, which is to say not at all.
//!
//! All three draw one quad covering exactly their own tile, so there is nothing outside the tile
//! square for a clip to cut — and all three read a source looked up at *its* own zoom, which is
//! what makes the clip worse than useless. A maxzoom-12 DEM under z14 vector layers puts z12
//! tiles in the frame; a 256-pixel one puts z15 tiles there. Either way both zooms' masks land
//! in one stencil byte, the finer replaces the coarser, and whichever zoom lost is rejected
//! everywhere the other covers. Over Hong Kong at z14.6 that left the background showing through
//! 61.4% of the frame where the relief and the hillshade should be.
//!
//! `Content::Raster` and `Content::Terrain` were both fixed for this; these two were missed.
//!
//! The bit is the single decision point: `frame::write_layer_state` emits a layer's clip set only
//! if some drawable of it carries `ENABLE_STENCIL`, so a drawable that does not carry one is also
//! a layer whose masks are never painted.

use std::sync::Arc;

use tessella_capture_abi::envelope::{DrawFlags, ViewId};
use tessella_layout::raster::RasterBucket;
use tessella_orchestrate::order;
use tessella_orchestrate::tile::{
    ColorReliefContent, Content, HillshadeContent, LayerBucket, RasterContent,
};
use tessella_orchestrate::view;
use tessella_source::dem::{Dem, Encoding};
use tessella_source::image::Image;

/// A square image, which is all `Dem::new` asks of one.
fn image() -> Image {
    Image {
        width: 4,
        height: 4,
        pixels: vec![0; 4 * 4 * 4],
    }
}

/// One bucket of the given content, at the given layer index.
fn bucket(layer_index: usize, content: Content) -> LayerBucket {
    LayerBucket {
        data_rev: 0,
        layer_index,
        layer_id: format!("layer-{layer_index}"),
        content,
        paint: Default::default(),
        pattern_vertices: Default::default(),
        outline_under_fill: false,
        binder: Default::default(),
        features: Vec::new(),
    }
}

/// The three raster-quad families, in layer order: raster, hillshade, color relief.
fn buckets() -> Vec<LayerBucket> {
    let dem = Arc::new(Dem::new(&image(), Encoding::Terrarium).expect("a square dem"));
    vec![
        bucket(
            0,
            Content::Raster(RasterContent {
                bucket: RasterBucket::whole_tile(),
                image: Arc::new(image()),
            }),
        ),
        bucket(
            1,
            Content::Hillshade(HillshadeContent {
                bucket: RasterBucket::whole_tile(),
                prepared: Arc::new(dem.prepare(12)),
                lat_range: [22.3, 22.2],
                dem: Arc::clone(&dem),
            }),
        ),
        bucket(
            2,
            Content::ColorRelief(ColorReliefContent {
                bucket: RasterBucket::whole_tile(),
                dem,
            }),
        ),
    ]
}

/// Neither DEM family asks for a clip, and the raster beside them does not either.
///
/// The raster is the control that matters: it is the family these two are supposed to match, and
/// it has carried `raster_flags` since tessella#331.
#[test]
fn a_hillshade_and_a_color_relief_carry_no_stencil_bit() {
    let mut next_id = 0;
    let bindings = order::bindings_for(
        ViewId(0),
        order::tile_of(12, 1673, 895),
        &buckets(),
        &mut next_id,
        true,
    );

    assert_eq!(bindings.len(), 3, "one drawable each");
    for (binding, what) in bindings.iter().zip(["raster", "hillshade", "color relief"]) {
        assert!(
            !binding.flags.contains(DrawFlags::ENABLE_STENCIL),
            "{what} asks to be clipped to its tile, so its masks are painted into the shared \
             stencil byte beside the vector cover's and one of the two zooms is rejected"
        );
    }
}

/// And they are clipped the way a raster is, rather than merely lacking one bit of its flags.
#[test]
fn the_dem_families_draw_with_the_rasters_flags() {
    let mut next_id = 0;
    let bindings = order::bindings_for(
        ViewId(0),
        order::tile_of(12, 1673, 895),
        &buckets(),
        &mut next_id,
        true,
    );

    for (binding, what) in bindings.iter().zip(["raster", "hillshade", "color relief"]) {
        assert_eq!(
            binding.flags,
            view::raster_flags(),
            "{what} does not draw with the raster flags"
        );
        assert_eq!(
            binding.pass,
            view::fill_pass(),
            "{what}: the translucent pass"
        );
    }
}

/// The absence above is a decision and not a flag set nobody sets.
///
/// `tiled_flags` is what these two carried, and a fill and a line carry it still, so the bit this
/// asserts away is live. Without this a `DrawFlags` that had lost `ENABLE_STENCIL` altogether
/// would pass both tests above.
#[test]
fn the_stencil_bit_is_still_carried_by_a_tiled_layer() {
    assert!(
        view::tiled_flags().contains(DrawFlags::ENABLE_STENCIL),
        "tiled layers are still clipped"
    );
    assert!(!view::raster_flags().contains(DrawFlags::ENABLE_STENCIL));
}
