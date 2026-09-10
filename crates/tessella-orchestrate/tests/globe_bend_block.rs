// SPDX-License-Identifier: Apache-2.0

//! The anchored bend's block — plan.md §18 item 6, producer half.
//!
//! `tessella-tile` pins the expansion against the exact chain. This pins the part that crosses the
//! wire: that the block carries those coefficients, that the depth bias lands where the direct bend
//! puts it, and that a buffer of them is indexable by drawable.

use tessella_capture_abi::globe_ubo::{GlobeBendUbo, ID_GLOBE_BEND_UBO};
use tessella_orchestrate::ubo::{self, depth_offset};
use tessella_tile::camera;
use tessella_tile::cover::ViewTransform;
use tessella_tile::globe;

fn view() -> ViewTransform {
    camera::settled(&ViewTransform {
        longitude: -121.8947,
        latitude: 36.6002,
        zoom: 13.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    })
}

const TILE: (u8, u32, u32, i32) = (13, 1310, 3165, 0);

/// The block is `anchored_bend`'s coefficients, cast once and not otherwise touched.
#[test]
fn the_block_carries_the_expansion() {
    let (z, x, y, wrap) = TILE;
    let bend = globe::anchored_bend(&view(), z, x, y, wrap);
    let block = ubo::globe_bend_block(&view(), z, x, y, wrap, 0, 0);

    // Scaled by `clip_w_scale`, which is what puts `w` in the units the symbol path reads it in.
    // A projective coordinate is scale-invariant in `x / w`, so this is not a change of position.
    let scale = globe::clip_w_scale(&view());
    #[allow(clippy::cast_possible_truncation)]
    let want = |v: [f64; 4]| -> [f32; 4] { core::array::from_fn(|i| (v[i] * scale) as f32) };
    assert_eq!(block.d_u, want(bend.d_u));
    assert_eq!(block.d_v, want(bend.d_v));
    assert_eq!(block.d_uu, want(bend.d_uu));
    assert_eq!(block.d_vv, want(bend.d_vv));
    assert_eq!(block.d_uv, want(bend.d_uv));
    // Only `z` differs, and only by the bias.
    assert_eq!(block.anchor[0], want(bend.anchor)[0]);
    assert_eq!(block.anchor[1], want(bend.anchor)[1]);
    assert_eq!(block.anchor[3], want(bend.anchor)[3]);
}

/// `w` arrives in the units a plane's `w` is in, which is what the symbol path reads it as.
///
/// It is the distance from the camera to the anchor, and `perspective_ratio` divides
/// `camera_to_center_distance` by it. `clip_matrix` measures that in sphere radii, so unscaled it
/// arrives near 0.001 against a plane's 1152 -- a ratio of a million, which pins the clamp at four
/// and makes every collision box four times its size. Every label in the frame then collides with
/// every other: 12 glyph quads drawn where a plane draws 1384.
#[test]
fn the_blocks_w_is_a_distance_a_plane_would_recognise() {
    let (z, x, y, wrap) = TILE;
    let view = view();
    let block = ubo::globe_bend_block(&view, z, x, y, wrap, 0, 0);
    let reference = camera::camera_to_center_distance(view.height);

    // At the tile's own centre the offsets are zero, so the anchor's `w` is the whole of it.
    let at_centre = f64::from(block.anchor[3]);
    let ratio = at_centre / reference;
    assert!(
        (0.5..2.0).contains(&ratio),
        "w is {at_centre} where a plane's is about {reference}, a ratio of {ratio}"
    );
}

/// The bias is the direct bend's, scaled to the same frustum, moved from the placement into `z`.
///
/// The direct path parks it in the placement's `[14]` and lets the shader carry it to clip `z`.
/// There is no placement in the anchored form, so it has to arrive already applied -- and if it
/// arrived unscaled, or with the other sign, coincident layers would either z-fight or vanish
/// through the near plane, which is the bug scaling it fixed in the first place.
#[test]
fn the_depth_bias_lands_in_the_anchor() {
    let (z, x, y, wrap) = TILE;
    let bend = globe::anchored_bend(&view(), z, x, y, wrap);
    let (near, far) = globe::depth_range(&view());

    let mut moved = 0;
    for (layer, sub) in [(0, 0), (3, 1), (40, 0)] {
        let block = ubo::globe_bend_block(&view(), z, x, y, wrap, layer, sub);
        // The bias goes on before the scale: the offset that reaches NDC is `bias / w`, and
        // scaling multiplies `w` too, so a bias added afterwards would land smaller by exactly
        // that factor -- a layer separation quietly reduced to nothing.
        let bias = -f64::from(depth_offset(layer, sub)) * (far - near);
        #[allow(clippy::cast_possible_truncation)]
        let want = ((bend.anchor[2] + bias) * globe::clip_w_scale(&view())) as f32;
        assert_eq!(block.anchor[2], want, "layer {layer}/{sub}");
        if layer > 0 {
            moved += 1;
        }
    }
    assert_eq!(moved, 2, "two layers past the first were checked");

    // And two layers really do separate, which is the whole point of the nudge.
    let low = ubo::globe_bend_block(&view(), z, x, y, wrap, 0, 0);
    let high = ubo::globe_bend_block(&view(), z, x, y, wrap, 40, 0);
    assert_ne!(low.anchor[2], high.anchor[2]);
}

/// A buffer is the blocks end to end at the stride, indexable by drawable.
#[test]
fn the_buffer_is_indexable_by_drawable() {
    let (z, x, y, wrap) = TILE;
    let blocks: Vec<GlobeBendUbo> = (0..4)
        .map(|layer| ubo::globe_bend_block(&view(), z, x, y, wrap, layer, 0))
        .collect();
    let bytes = ubo::pack_globe_bend_buffer(&blocks);
    let stride = GlobeBendUbo::STRIDE as usize;
    assert_eq!(bytes.len(), blocks.len() * stride);
    for (index, block) in blocks.iter().enumerate() {
        assert_eq!(
            &bytes[index * stride..(index + 1) * stride],
            &block.to_bytes(),
            "drawable {index} is not at its own offset"
        );
    }
}

/// The slot is one nothing else on a fill claims, which is what lets this ride an existing channel.
#[test]
fn the_slot_collides_with_nothing() {
    assert_eq!(ID_GLOBE_BEND_UBO, 11);
    assert_ne!(ID_GLOBE_BEND_UBO, ubo::drawable_slot());
}
