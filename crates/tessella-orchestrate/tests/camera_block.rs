//! The camera block, checked against the golden dump (§6.3, §9.1).
//!
//! Every field the oracle reports is compared as bits: the sixteen-element projection, the
//! scale-free center, bearing, pitch, `pixelsPerMeter`, the light, and the depth range. Nothing
//! here is a tolerance — the dump carries bit patterns and so does this.

use tessella_capture_abi::envelope::{OrderEpoch, ViewId};
use tessella_capture_abi::{CameraMode, EnvelopeKind};
use tessella_orchestrate::camera::CameraBlock;
use tessella_style::light::Light;
use tessella_tile::cover::ViewTransform;

const DUMP: &str = include_str!("../../../tests/golden/hermetic_style.dump");

fn probe() -> ViewTransform {
    ViewTransform {
        longitude: -0.11,
        latitude: 51.505,
        zoom: 13.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    }
}

fn block() -> CameraBlock {
    CameraBlock::new(&probe(), &Light::default(), OrderEpoch(1), 0, 0).expect("an unrotated camera")
}

/// Pulls the hex words following a marker in the dump's camera section.
fn words(marker: &str) -> Vec<u64> {
    DUMP.lines()
        .filter_map(|line| line.trim_start().strip_prefix(marker))
        .flat_map(|rest| {
            rest.split_whitespace()
                .filter_map(|word| u64::from_str_radix(word, 16).ok())
        })
        .collect()
}

/// The sixteen projection elements the dump lists one per line.
fn oracle_proj() -> Vec<u64> {
    let mut values = Vec::new();
    for line in DUMP.lines() {
        let Some(rest) = line.trim_start().strip_prefix("proj ") else {
            continue;
        };
        let word = rest.split_whitespace().nth(1).expect("a bit pattern");
        values.push(u64::from_str_radix(word, 16).expect("hex"));
    }
    values
}

/// The projection reproduces the oracle's, element for element.
#[test]
fn the_projection_matches_the_oracle() {
    let oracle = oracle_proj();
    assert_eq!(oracle.len(), 16, "the dump lists sixteen elements");
    let record = block().record;
    for (index, want) in oracle.iter().enumerate() {
        assert_eq!(
            record.proj_matrix[index].to_bits(),
            *want,
            "element {index}: {:?}",
            record.proj_matrix[index]
        );
    }
}

/// The scale-free center, bearing, pitch and `pixelsPerMeter` all match as bits.
#[test]
fn the_camera_scalars_match_the_oracle() {
    let record = block().record;
    let center = words("centerZoom0 ");
    assert_eq!(center.len(), 2);
    assert_eq!(record.center_zoom0[0].to_bits(), center[0]);
    assert_eq!(record.center_zoom0[1].to_bits(), center[1]);

    assert_eq!(record.bearing.to_bits(), words("bearing ")[0]);
    assert_eq!(record.pitch.to_bits(), words("pitch ")[0]);
    assert_eq!(
        record.pixels_per_meter.to_bits(),
        words("pixelsPerMeter ")[0]
    );
}

/// The light matches: direction, color, intensity and anchor.
#[test]
fn the_light_matches_the_oracle() {
    let record = block().record;

    let direction = words("light dir ");
    assert_eq!(direction.len(), 3);
    for (index, want) in direction.iter().enumerate() {
        assert_eq!(
            record.light.direction[index].to_bits(),
            *want,
            "direction {index}"
        );
    }

    let color = words("light color ");
    assert_eq!(color.len(), 4);
    for (index, want) in color.iter().enumerate() {
        assert_eq!(record.light.color[index].to_bits(), *want, "color {index}");
    }

    assert_eq!(
        record.light.intensity.to_bits(),
        words("light intensity ")[0]
    );
    assert_eq!(
        record.light.anchored_to_map, 0,
        "the oracle's light is viewport-anchored"
    );
}

/// The depth range matches what the oracle reports for its frame.
#[test]
fn the_depth_range_matches_the_oracle() {
    let want = DUMP
        .lines()
        .find_map(|line| line.strip_prefix("camera cutoff="))
        .and_then(|rest| rest.split("depthRange=").nth(1))
        .map(|word| u32::from_str_radix(word.trim(), 16).expect("hex"))
        .expect("a depth range");
    assert_eq!(block().record.depth_range_size.to_bits(), want);
}

/// Consumer-camera mode zeroes the projection rather than sending a producer-computed one.
///
/// A consumer that mistakenly applied a zero matrix draws nothing, which is a bug that gets
/// found. One that applied a plausible but non-authoritative matrix draws the wrong thing
/// convincingly, which is a bug that ships.
#[test]
fn consumer_camera_mode_zeroes_the_projection() {
    let producer_mode = block().for_mode(CameraMode::Producer).record;
    let consumer_mode = block().for_mode(CameraMode::Consumer).record;

    assert_eq!(consumer_mode.proj_matrix, [0.0; 16]);
    assert_eq!(consumer_mode.center_zoom0, [0.0; 2]);
    assert_ne!(
        producer_mode.proj_matrix, [0.0; 16],
        "and producer mode does not"
    );

    // What has no other home still travels in both modes.
    assert_eq!(consumer_mode.light, producer_mode.light);
    assert_eq!(consumer_mode.order_epoch, producer_mode.order_epoch);
    assert_eq!(
        consumer_mode.pixels_per_meter, producer_mode.pixels_per_meter,
        "the producer still needs this for screen-space sizing"
    );
}

/// The block names the epoch it was computed against, which is what section 4's
/// hold-camera-until-order rule is stated in terms of.
#[test]
fn the_block_carries_its_order_epoch() {
    for epoch in [0u64, 1, 7, u64::MAX] {
        let block = CameraBlock::new(&probe(), &Light::default(), OrderEpoch(epoch), 3, 2)
            .expect("an unrotated camera");
        assert_eq!(block.record.order_epoch, OrderEpoch(epoch));
        assert_eq!(block.record.frame_no, 3);
        assert_eq!(block.record.opaque_pass_cutoff, 2);
    }
}

/// The block goes on the ring as one envelope with no payload.
#[test]
fn the_block_writes_one_envelope() {
    use tessella_capture_abi::ring::Ring;

    let mut ring = Ring::new(1 << 14);
    let (producer, consumer) = ring.split();
    block().for_view(ViewId(2)).write(producer).expect("writes");

    let record = consumer.peek().expect("an envelope");
    assert_eq!(record.kind, EnvelopeKind::CameraUpdate);
    assert_eq!(record.payload.len(), 0, "the camera block has no payload");
    assert_eq!(
        record.record.len(),
        core::mem::size_of::<tessella_capture_abi::envelope::CameraUpdate>()
    );
}
