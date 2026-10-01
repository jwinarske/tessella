//! Pairing a geometry announcement with the views that use it.
//!
//! The cases that matter are the ones where the two halves do not arrive in the obvious order, or
//! arrive more than once. Those are not hypothetical: the producer sends a use when a drawable
//! enters a view's cover and not again, and re-announces geometry whose attributes changed.

use tessella_capture_abi::RenderPass;
use tessella_capture_abi::envelope::{
    AttributeDesc, DrawFlags, GeometryAdd, GeometryId, SlabRef, Span, TileId, ViewId, ViewUse,
};
use tessella_consume::join::{Announcement, Joiner};

fn announcement(id: u64, at: u64, attrs: &[AttributeDesc]) -> Announcement {
    Announcement {
        add: GeometryAdd {
            geometry: GeometryId(id),
            permutation_key: 0,
            indexes: SlabRef::default(),
            vertex_count: 3,
            attrs: Span::default(),
            instance_attrs: Span::default(),
            segments: Span::default(),
            texture_refs: Span::default(),
            builtin_shader: 0,
            vertex_type: 0,
            reason: 0,
            topology: 0,
            _pad: [0; 1],
        },
        announced_at: at,
        attrs: attrs.to_vec(),
        instance_attrs: Vec::new(),
        segments: Vec::new(),
        texture_refs: Vec::new(),
    }
}

fn use_of(id: u64, view: u32, layer: i32) -> ViewUse {
    ViewUse {
        geometry: GeometryId(id),
        view: ViewId(view),
        layer_index: layer,
        sub_layer_index: 0,
        tile: TileId::default(),
        render_pass: RenderPass::default(),
        draw_flags: DrawFlags::default(),
        has_tile: 0,
        _pad: [0; 5],
    }
}

/// The ordinary order: the announcement, then a view using it.
#[test]
fn a_use_after_an_announcement_draws() {
    let mut joiner = Joiner::new();
    joiner.announce(announcement(1, 100, &[])).for_each(drop);
    assert!(joiner.used(use_of(1, 7, 3)), "both halves are in");

    let drawable = joiner
        .drawable(GeometryId(1), ViewId(7))
        .expect("a drawable");
    assert_eq!(drawable.use_.layer_index, 3);
    assert_eq!(drawable.geometry.announced_at, 100);
}

/// And the other order, which is as ordinary: a use can arrive first.
#[test]
fn a_use_before_an_announcement_waits_and_then_draws() {
    let mut joiner = Joiner::new();
    assert!(
        !joiner.used(use_of(1, 7, 3)),
        "nothing to draw until the geometry arrives"
    );
    assert!(joiner.drawable(GeometryId(1), ViewId(7)).is_none());

    let views: Vec<ViewId> = joiner.announce(announcement(1, 100, &[])).collect();
    assert_eq!(views, [ViewId(7)], "the waiting view is reported");
    assert!(joiner.drawable(GeometryId(1), ViewId(7)).is_some());
}

/// Four views over one geometry are four drawables and one announcement.
#[test]
fn one_geometry_serves_every_view_that_uses_it() {
    let mut joiner = Joiner::new();
    joiner.announce(announcement(1, 100, &[])).for_each(drop);
    for view in 0..4 {
        joiner.used(use_of(1, view, i32::try_from(view).unwrap()));
    }

    assert_eq!(joiner.geometries(), 1, "one announcement");
    assert_eq!(joiner.uses(), 4, "four uses");
    for view in 0..4 {
        let drawable = joiner
            .drawable(GeometryId(1), ViewId(view))
            .expect("a drawable per view");
        assert_eq!(
            drawable.use_.layer_index,
            i32::try_from(view).unwrap(),
            "each view keeps its own layer index"
        );
    }
}

/// A re-announcement reaches every view already using the geometry.
///
/// The case the C++ gets half right: it holds one use per geometry, so a re-announcement re-joins
/// whichever view was seen last and the others keep the old geometry. Invisible with one view, and
/// the quad's ordinary case with four.
#[test]
fn a_re_announcement_reaches_every_view_not_just_the_last() {
    let mut joiner = Joiner::new();
    joiner.announce(announcement(1, 100, &[])).for_each(drop);
    joiner.used(use_of(1, 0, 0));
    joiner.used(use_of(1, 1, 1));

    let changed: Vec<ViewId> = joiner.announce(announcement(1, 200, &[])).collect();
    assert_eq!(
        changed,
        [ViewId(0), ViewId(1)],
        "both views hold the re-announced geometry"
    );
    for view in 0..2 {
        assert_eq!(
            joiner
                .drawable(GeometryId(1), ViewId(view))
                .expect("a drawable")
                .geometry
                .announced_at,
            200,
            "and each sees the new announcement"
        );
    }
}

/// The runs are read out of the payload, so a re-announcement replaces them.
#[test]
fn a_re_announcement_replaces_the_attribute_run() {
    let first = AttributeDesc {
        attr_id: 1,
        binding: 0,
        source: SlabRef::default(),
        offset: 0,
        vertex_offset: 0,
        stride: 8,
        data_type: 0,
        declared_data_type: 0,
        _pad: [0; 2],
    };
    let mut second = first;
    second.stride = 16;

    let mut joiner = Joiner::new();
    joiner
        .announce(announcement(1, 100, &[first]))
        .for_each(drop);
    joiner.used(use_of(1, 0, 0));
    joiner
        .announce(announcement(1, 200, &[second]))
        .for_each(drop);

    let drawable = joiner
        .drawable(GeometryId(1), ViewId(0))
        .expect("a drawable");
    assert_eq!(
        drawable.geometry.attrs,
        [second],
        "the new run, not the old"
    );
}

/// A second use from the same view replaces the first rather than accumulating.
#[test]
fn a_view_using_a_geometry_twice_keeps_one_use() {
    let mut joiner = Joiner::new();
    joiner.announce(announcement(1, 100, &[])).for_each(drop);
    joiner.used(use_of(1, 0, 3));
    joiner.used(use_of(1, 0, 9));

    assert_eq!(joiner.uses(), 1, "one view, one use");
    assert_eq!(
        joiner
            .drawable(GeometryId(1), ViewId(0))
            .expect("a drawable")
            .use_
            .layer_index,
        9,
        "the later use wins"
    );
}

/// Releasing one view's hold leaves the geometry for the views that remain.
///
/// `ViewRelease` and `GeometryRemove` are distinct here where mbgl had one record. Treating them
/// alike retires geometry that three other views are still drawing.
#[test]
fn releasing_one_view_leaves_the_others_drawing() {
    let mut joiner = Joiner::new();
    joiner.announce(announcement(1, 100, &[])).for_each(drop);
    joiner.used(use_of(1, 0, 0));
    joiner.used(use_of(1, 1, 1));

    assert!(
        joiner.release(GeometryId(1), ViewId(0)),
        "the hold was there"
    );
    assert!(joiner.drawable(GeometryId(1), ViewId(0)).is_none());
    assert!(
        joiner.drawable(GeometryId(1), ViewId(1)).is_some(),
        "the other view still draws"
    );
    assert_eq!(joiner.geometries(), 1, "and the geometry is still held");
}

/// Releasing a hold nobody has is not an error.
#[test]
fn releasing_what_is_not_held_is_survivable() {
    let mut joiner = Joiner::new();
    assert!(!joiner.release(GeometryId(9), ViewId(0)));
    joiner.announce(announcement(1, 100, &[])).for_each(drop);
    joiner.used(use_of(1, 0, 0));
    assert!(
        !joiner.release(GeometryId(1), ViewId(5)),
        "a view that never used it"
    );
    assert!(joiner.drawable(GeometryId(1), ViewId(0)).is_some());
}

/// Retiring takes the geometry and every view's use with it.
#[test]
fn retiring_takes_the_uses_too() {
    let mut joiner = Joiner::new();
    joiner.announce(announcement(1, 100, &[])).for_each(drop);
    joiner.used(use_of(1, 0, 0));
    joiner.used(use_of(1, 1, 0));

    assert!(joiner.retire(GeometryId(1)), "it was held");
    assert_eq!(joiner.geometries(), 0);
    assert_eq!(joiner.uses(), 0, "no use outlives the geometry it names");
    assert!(joiner.drawable(GeometryId(1), ViewId(0)).is_none());
}

/// Retiring something never announced is not an error either.
///
/// A consumer can join the stream after a geometry was announced and before it retires, and a
/// stream is another process's: refusing it would make a late attach fatal.
#[test]
fn retiring_what_was_never_announced_is_survivable() {
    let mut joiner = Joiner::new();
    assert!(!joiner.retire(GeometryId(4)));
}

/// Every view's drawable for one geometry, which is what a frame walks.
#[test]
fn drawables_reports_one_per_view() {
    let mut joiner = Joiner::new();
    joiner.announce(announcement(1, 100, &[])).for_each(drop);
    joiner.used(use_of(1, 2, 0));
    joiner.used(use_of(1, 5, 1));

    let views: Vec<ViewId> = joiner
        .drawables(GeometryId(1))
        .map(|drawable| drawable.use_.view)
        .collect();
    assert_eq!(views, [ViewId(2), ViewId(5)]);
    assert_eq!(
        joiner.drawables(GeometryId(9)).count(),
        0,
        "and none for a stranger"
    );
}

/// A use held for a geometry that never arrives draws nothing and is not lost.
#[test]
fn a_use_waiting_on_an_announcement_that_never_comes_draws_nothing() {
    let mut joiner = Joiner::new();
    joiner.used(use_of(1, 0, 0));
    assert_eq!(joiner.drawables(GeometryId(1)).count(), 0);
    assert_eq!(joiner.uses(), 1, "still held, in case it arrives");
    assert_eq!(joiner.geometries(), 0);
}
