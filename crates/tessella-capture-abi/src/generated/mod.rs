//! Generated mirrors of mbgl C++ types (DR-6).
//!
//! Nothing in here is written by hand. Each file records the maplibre-native revision it came
//! from in its header; `mbgl-codegen` rewrites them all from the pinned tree.

pub mod mbgl_enums;
pub mod shader_attributes;
pub mod ubo_layouts;

#[cfg(test)]
mod tests {
    use super::mbgl_enums::{AttributeDataType, BuiltIn};
    use super::shader_attributes::{attributes, declared_for};

    /// Checked against the golden dump's data-driven fill drawable, which emits:
    ///
    /// ```text
    /// id=0 bind=0  dt=9  ddt=9    stride=4
    /// id=1 bind=1  dt=26 ddt=28   stride=20 off=0
    /// id=2 bind=2  dt=25 ddt=26   stride=20 off=8
    /// id=3 bind=-1 dt=26 ddt=255  stride=20 off=12
    /// ```
    ///
    /// Those declared types are what this table has to supply, and 9/26/28 decode as
    /// `Short2`, `Float2` and `Float4`.
    #[test]
    fn the_fill_table_matches_the_oracle() {
        let position = declared_for(BuiltIn::FillShader, 0).expect("position");
        assert_eq!(position.binding, 0);
        assert_eq!(position.declared, AttributeDataType::Short2);

        // fill-color: the shader declares the packed min/max pair, Float4, while the binder
        // supplies Float2 for a property that varies per feature but not with zoom (§2.2).
        let color = declared_for(BuiltIn::FillShader, 1).expect("color");
        assert_eq!(color.binding, 1);
        assert_eq!(color.declared, AttributeDataType::Float4);

        let opacity = declared_for(BuiltIn::FillShader, 2).expect("opacity");
        assert_eq!(opacity.binding, 2);
        assert_eq!(opacity.declared, AttributeDataType::Float2);
    }

    /// The drop-undeclared-override rule, which the golden dump shows as `bind=-1 ddt=255`.
    ///
    /// `fill-outline-color` is attribute 3 and the plain fill shader has no slot for it, so a
    /// drawable supplying it must bind at -1 and the consumer must drop it. A table that
    /// invented a slot here would bind the outline color over something else.
    #[test]
    fn an_undeclared_attribute_has_no_slot() {
        assert_eq!(declared_for(BuiltIn::FillShader, 3), None);

        // The fill *outline* shader does declare it, at binding 1 — the same slot the plain
        // shader gives to fill-color, which is why the two cannot share a table.
        let outline = declared_for(BuiltIn::FillOutlineShader, 3).expect("outline color");
        assert_eq!(outline.binding, 1);
        assert_eq!(outline.declared, AttributeDataType::Float4);
    }

    /// Position is attribute zero for every shader that has one, which is what lets a producer
    /// emit geometry before it knows anything else about the shader.
    #[test]
    fn position_is_attribute_zero() {
        for shader in [
            BuiltIn::FillShader,
            BuiltIn::FillOutlineShader,
            BuiltIn::LineShader,
            BuiltIn::CircleShader,
        ] {
            let first = attributes(shader).first().expect("at least one attribute");
            assert_eq!(first.attr_id, 0, "{shader:?}");
            assert_eq!(first.binding, 0, "{shader:?}");
        }
    }

    /// Binding slots within a shader are distinct. Two attributes sharing one would bind one
    /// over the other.
    #[test]
    fn binding_slots_are_unique_within_a_shader() {
        for shader in BuiltIn::ALL {
            let table = attributes(shader);
            for (i, a) in table.iter().enumerate() {
                for b in &table[i + 1..] {
                    assert_ne!(
                        a.binding, b.binding,
                        "{shader:?}: {} and {}",
                        a.name, b.name
                    );
                    assert_ne!(
                        a.attr_id, b.attr_id,
                        "{shader:?}: {} and {}",
                        a.name, b.name
                    );
                }
            }
        }
    }

    /// A shader with no table is a shader this build has no data for, not one that binds
    /// nothing. The background shader genuinely has attributes; a truly absent one is the
    /// signal that generation missed something.
    #[test]
    fn the_shaders_r0_needs_have_tables() {
        assert!(!attributes(BuiltIn::BackgroundShader).is_empty());
        assert!(!attributes(BuiltIn::FillShader).is_empty());
        assert!(!attributes(BuiltIn::FillOutlineShader).is_empty());
    }

    /// Every generated layout is internally consistent: fields in order, no gaps, no overlaps,
    /// and a size that is the sum of them.
    ///
    /// The generator checks this before emitting, so a failure here means the emitted form lost
    /// something the parse had — which is a different bug from a header the parse misread, and
    /// worth being able to tell apart.
    #[test]
    fn every_layout_is_internally_consistent() {
        use super::ubo_layouts::LAYOUTS;

        for layout in LAYOUTS {
            let mut running = 0;
            for field in layout.fields {
                assert_eq!(
                    field.offset, running,
                    "{}: field `{}` at {} where the previous field ends at {running}",
                    layout.name, field.name, field.offset
                );
                running += field.kind.size();
            }
            assert_eq!(running, layout.size, "{}", layout.name);
            assert_eq!(layout.align, 16, "{} is not 16-aligned", layout.name);

            // `size` is where the fields end; `stride` is `sizeof`, which pads up to the
            // alignment. They differ only when the fields end mid-alignment —
            // `SymbolDrawableUBO` ends at 260 and occupies 272 — and it is the stride that
            // separates consecutive blocks in a consolidated buffer. Packing at `size` would
            // put every block after the first at the wrong offset.
            assert!(layout.stride >= layout.size, "{}", layout.name);
            assert!(layout.stride - layout.size < 16, "{}", layout.name);
            assert_eq!(layout.stride % 16, 0, "{}", layout.name);
        }
    }

    /// The blocks R0 needs are present and sized as the oracle's UBO writes are.
    ///
    /// The golden dump reports `ubo layer:0 slot=5 size=32` for the background layer's
    /// properties and `slot=5 size=48` for a fill layer's, which are exactly
    /// `BackgroundPropsUBO` and `FillEvaluatedPropsUBO`. A layout that had drifted from the
    /// shaders would disagree here before it ever mispacked a frame.
    #[test]
    fn the_blocks_r0_needs_match_the_oracles_sizes() {
        use super::ubo_layouts::{
            BACKGROUND_DRAWABLE_UBO, BACKGROUND_PROPS_UBO, FILL_DRAWABLE_UBO,
            FILL_EVALUATED_PROPS_UBO, FILL_OUTLINE_DRAWABLE_UBO,
        };

        assert_eq!(BACKGROUND_PROPS_UBO.size, 32);
        assert_eq!(FILL_EVALUATED_PROPS_UBO.size, 48);
        assert_eq!(BACKGROUND_DRAWABLE_UBO.size, 64);

        // The fill and fill-outline drawable blocks are the same size, which is not obvious —
        // the outline carries a different second interpolation, not an extra one.
        assert_eq!(FILL_DRAWABLE_UBO.size, 80);
        assert_eq!(FILL_OUTLINE_DRAWABLE_UBO.size, 80);

        // None of R0's blocks needs padding: their fields land on the alignment already.
        for layout in [
            BACKGROUND_PROPS_UBO,
            FILL_EVALUATED_PROPS_UBO,
            BACKGROUND_DRAWABLE_UBO,
            FILL_DRAWABLE_UBO,
            FILL_OUTLINE_DRAWABLE_UBO,
        ] {
            assert_eq!(layout.stride, layout.size, "{}", layout.name);
        }
    }

    /// A block whose fields end mid-alignment has a stride larger than its size, and mbgl's own
    /// `static_assert` is what says so. The generator cross-checks against that assert, so this
    /// is the case proving the two quantities are genuinely distinct rather than always equal.
    #[test]
    fn a_block_ending_mid_alignment_is_padded() {
        use super::ubo_layouts::LAYOUTS;

        let padded: Vec<&str> = LAYOUTS
            .iter()
            .filter(|layout| layout.stride != layout.size)
            .map(|layout| layout.name)
            .collect();
        assert!(
            padded.contains(&"SymbolDrawableUBO"),
            "expected a padded block among {padded:?}"
        );
        let symbol = LAYOUTS
            .iter()
            .find(|layout| layout.name == "SymbolDrawableUBO")
            .expect("present");
        assert_eq!(symbol.size, 260, "fields end here");
        assert_eq!(symbol.stride, 272, "and mbgl asserts sizeof is 17 * 16");
    }

    /// A drawable block starts with the tile matrix, which is what makes the placement the
    /// consumer applies the same one `StencilTiles` describes.
    #[test]
    fn drawable_blocks_lead_with_the_matrix() {
        use super::ubo_layouts::{
            BACKGROUND_DRAWABLE_UBO, FILL_DRAWABLE_UBO, FILL_OUTLINE_DRAWABLE_UBO, UboFieldKind,
        };

        for layout in [
            BACKGROUND_DRAWABLE_UBO,
            FILL_DRAWABLE_UBO,
            FILL_OUTLINE_DRAWABLE_UBO,
        ] {
            let first = layout.fields.first().expect("at least one field");
            assert_eq!(first.name, "matrix", "{}", layout.name);
            assert_eq!(first.offset, 0, "{}", layout.name);
            assert_eq!(first.kind, UboFieldKind::Mat4, "{}", layout.name);
        }
    }

    /// What the generator would not vouch for is named rather than omitted, and none of it is
    /// a block R0 needs.
    ///
    /// The distinction matters: a block missing because mbgl has no such thing and one missing
    /// because a header could not be read are different situations, and only the second is a
    /// reason to go and look at the header.
    #[test]
    fn unparsed_blocks_are_named_and_none_are_r0() {
        use super::ubo_layouts::UNPARSED;

        for (name, header, reason) in UNPARSED {
            assert!(!reason.is_empty(), "{name} has no reason");
            assert!(
                name.starts_with("Line") || name.starts_with("Symbol"),
                "{name} in {header} is outside line and symbol work: {reason}"
            );
        }
    }

    /// Field kinds report the sizes their C++ types occupy.
    #[test]
    fn field_kinds_are_sized_as_their_types() {
        use super::ubo_layouts::UboFieldKind;

        assert_eq!(UboFieldKind::F32.size(), 4);
        assert_eq!(UboFieldKind::I32.size(), 4);
        assert_eq!(UboFieldKind::U32.size(), 4);
        assert_eq!(UboFieldKind::Vec2.size(), 8);
        assert_eq!(UboFieldKind::Vec3.size(), 12);
        assert_eq!(UboFieldKind::Vec4.size(), 16);
        assert_eq!(UboFieldKind::Color.size(), 16);
        assert_eq!(UboFieldKind::Mat4.size(), 64);
    }
}
