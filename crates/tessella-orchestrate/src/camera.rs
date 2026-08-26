//! Assembling and emitting the per-view camera block (§6.3, §11.1, DR-9).
//!
//! # What the camera block is for
//!
//! One envelope per view per frame carrying everything a consumer needs to draw the frame that
//! is not geometry: the projection, the scale-free center, the orientation, the light, and the
//! order epoch the camera was computed against.
//!
//! # The epoch is the whole point of the split
//!
//! DR-4 separated the camera from the order because they change at different rates. What makes
//! the pair safe is that a camera *names* the order it was computed against, and §4's
//! hold-camera-until-order rule tells a consumer what to do when it has not seen that order yet:
//! hold the camera rather than draw it against the previous one. R-5 is the failure when this
//! goes wrong, and it shows up as a single frame of flicker under churn — which is to say, as
//! something nobody can reproduce on demand.
//!
//! # Consumer-camera mode degrades this envelope rather than removing it
//!
//! Under DR-9 an interactive view's camera belongs to the consumer's ECS. The block still
//! travels: the non-matrix fields — light, epoch, cutoff, depth range — have no other home, and
//! the producer still computes a projection for its own cover and screen-space decisions. What
//! changes is which side is authoritative, and [`CameraBlock::for_mode`] is where that is
//! decided rather than being left to a reader of the stream to infer.

use tessella_capture_abi::CameraMode;
use tessella_capture_abi::EnvelopeKind;
use tessella_capture_abi::envelope::{
    CameraUpdate, Light as AbiLight, OrderEpoch, ViewId, WireRecord,
};
use tessella_capture_abi::ring::{Full, Producer};
use tessella_style::light::{Anchor, Light};
use tessella_tile::camera::{self, CameraError};
use tessella_tile::cover::ViewTransform;

/// A camera block, before it goes on the wire.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CameraBlock {
    /// The record as it will be written.
    pub record: CameraUpdate,
}

impl CameraBlock {
    /// Builds the block for a view.
    ///
    /// The transform is settled before the projection is computed, because a map does not store
    /// the center it is handed — see [`tessella_tile::camera::settled_center`]. Skipping that
    /// step gives a projection that is correct to a part in 10^14 and not bit-exact against a
    /// capture, which is the difference between a diff that passes and one that does not.
    ///
    /// # Errors
    ///
    /// [`CameraError::EmptyViewport`] when the view has no area.
    pub fn new(
        view: &ViewTransform,
        light: &Light,
        epoch: OrderEpoch,
        frame_no: u64,
        opaque_cutoff: u32,
    ) -> Result<Self, CameraError> {
        let settled = camera::settled(view);
        let proj_matrix = camera::proj_matrix(&settled)?;
        let cartesian = light.cartesian();

        Ok(Self {
            record: CameraUpdate {
                proj_matrix,
                center_zoom0: camera::center_zoom0(&settled),
                bearing: settled.bearing,
                pitch: settled.pitch,
                pixels_per_meter: camera::pixels_per_meter(&settled),
                light: AbiLight {
                    direction: [
                        f64::from(cartesian[0]),
                        f64::from(cartesian[1]),
                        f64::from(cartesian[2]),
                    ],
                    color: [
                        f64::from(light.color.r),
                        f64::from(light.color.g),
                        f64::from(light.color.b),
                        f64::from(light.color.a),
                    ],
                    intensity: f64::from(light.intensity),
                    anchored_to_map: u8::from(light.anchor == Anchor::Map),
                    _pad: [0; 7],
                },
                frame_no,
                order_epoch: epoch,
                view: ViewId(0),
                opaque_pass_cutoff: opaque_cutoff,
                // mbgl derives this from the layer group count; the oracle's frame reports
                // 0.98828125. Carried as measured rather than recomputed, because the formula
                // counts layer groups this frontend does not model.
                depth_range_size: DEFAULT_DEPTH_RANGE,
                _pad: 0,
            },
        })
    }

    /// The block for a view, with the mode's degradation applied.
    ///
    /// In consumer-camera mode the projection and center are zeroed: the consumer's ECS owns
    /// them and a producer-computed matrix on the wire is at best ignored and at worst applied.
    /// Zeroing is louder than sending a stale-but-plausible matrix — a consumer that mistakenly
    /// used it would draw nothing rather than draw the wrong thing convincingly.
    #[must_use]
    pub fn for_mode(mut self, mode: CameraMode) -> Self {
        if mode == CameraMode::Consumer {
            self.record.proj_matrix = [0.0; 16];
            self.record.center_zoom0 = [0.0; 2];
        }
        self
    }

    /// Names the view this block belongs to.
    #[must_use]
    pub const fn for_view(mut self, view: ViewId) -> Self {
        self.record.view = view;
        self
    }

    /// Writes the block to the ring.
    ///
    /// # Errors
    ///
    /// [`Full`] when the ring cannot take it.
    pub fn write(&self, producer: &mut Producer) -> Result<(), Full> {
        producer.write(EnvelopeKind::CameraUpdate, self.record.as_bytes(), &[])
    }
}

/// The depth range the oracle reports for its frame.
pub const DEFAULT_DEPTH_RANGE: f32 = 0.988_281_25;
