//! Generated mirrors of mbgl C++ types (DR-6).
//!
//! Nothing in here is written by hand. Each file records the maplibre-native revision it came
//! from in its header; `mbgl-codegen` rewrites them all from the pinned tree.

pub mod mbgl_enums;
pub mod shader_attributes;
pub mod texture_slots;
pub mod ubo_layouts;
pub mod ubo_slots;

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

    /// The texture tables agree with the golden dump on both sides of the question.
    ///
    /// `symbol_style.dump` gives each symbol drawable exactly one `tex ... slot=0` line and its
    /// fill drawables none at all. Both are reproduced here from the header and the shader
    /// sources rather than from the dump, which is what makes the agreement mean something: one
    /// says the SDF shader has a sampler at slot 0, the other says a plain fill has none, and a
    /// table that had drifted would disagree with the capture before it ever bound anything.
    #[test]
    fn the_texture_tables_match_the_oracles_bindings() {
        use super::texture_slots::{texture_count, textures};

        let symbol = textures(BuiltIn::SymbolSDFShader);
        assert_eq!(symbol.len(), 1, "the dump shows one tex line per symbol");
        assert_eq!(symbol[0].binding, 0, "slot=0");
        assert_eq!(symbol[0].name, "idSymbolImageTexture");

        assert_eq!(
            texture_count(BuiltIn::FillShader),
            Some(0),
            "the dump's fill drawables carry no tex line"
        );
        assert_eq!(texture_count(BuiltIn::BackgroundShader), Some(0));
    }

    /// A raster drawable binds *two* textures, both of them the tile's own image.
    ///
    /// Which looks like a mistake in `render_raster_layer.cpp` and is not: slot 1 is the parent
    /// tile a fading tile blends against, and with no fade in progress it is the same image. A
    /// producer binding only slot 0 leaves the second sampler unbound, and what a shader reads
    /// from an unbound sampler is the backend's business rather than a defined black.
    #[test]
    fn a_raster_shader_has_two_samplers_for_one_picture() {
        use super::texture_slots::textures;

        let raster = textures(BuiltIn::RasterShader);
        assert_eq!(raster.len(), 2);
        assert_eq!(raster[0].binding, 0);
        assert_eq!(raster[1].binding, 1);
        assert_eq!(raster[0].name, "idRasterImage0Texture");
        assert_eq!(raster[1].name, "idRasterImage1Texture");
    }

    /// The icon atlas has a slot only on the shader that declares one.
    ///
    /// `SymbolSDFShader` samples text alone; `SymbolTextAndIconShader` samples both. Binding the
    /// sprite atlas at slot 1 of the first would bind a texture that shader has no sampler for —
    /// which is the whole reason the slot comes from a table rather than from a remembered
    /// number.
    #[test]
    fn the_icon_atlas_has_no_slot_on_the_text_only_shader() {
        use super::texture_slots::textures;

        assert!(
            !textures(BuiltIn::SymbolSDFShader)
                .iter()
                .any(|texture| texture.name == "idSymbolImageIconTexture")
        );
        let both = textures(BuiltIn::SymbolTextAndIconShader);
        assert_eq!(both.len(), 2);
        assert_eq!(both[1].binding, 1);
        assert_eq!(both[1].name, "idSymbolImageIconTexture");
    }

    /// A shader that samples nothing is distinguishable from one with no table.
    ///
    /// Both answer `textures()` with an empty slice, and they mean opposite things: the first is
    /// a fill shader, correctly binding nothing, and the second is a gap in generation that
    /// would emit a drawable which cannot draw.
    #[test]
    fn an_empty_table_and_a_missing_one_are_different() {
        use super::texture_slots::{TABLED, texture_count, textures};

        assert_eq!(texture_count(BuiltIn::FillShader), Some(0));
        assert!(textures(BuiltIn::FillShader).is_empty());

        // Every shader in the match arm reports a count; anything outside it reports `None`.
        for shader in TABLED {
            assert!(texture_count(shader).is_some(), "{shader:?}");
        }
        assert_eq!(TABLED.len(), 29, "the vulkan shader sources declare 29");
    }

    /// Binding slots within a shader are distinct and dense from zero.
    ///
    /// A gap would mean the parse dropped an entry, and a duplicate would bind one sampler over
    /// another. Neither is visible in a table read one row at a time.
    #[test]
    fn texture_slots_are_dense_and_unique() {
        use super::texture_slots::{TABLED, textures};

        for shader in TABLED {
            let table = textures(shader);
            for (index, texture) in table.iter().enumerate() {
                assert_eq!(
                    usize::try_from(texture.binding).expect("a small slot"),
                    index,
                    "{shader:?}: {} is not at its position",
                    texture.name
                );
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

    /// The union strides explain every consolidated-buffer size the oracle reports.
    ///
    /// A consolidated buffer is an array of the *union*, not of the block a drawable happens to
    /// use: mbgl sizes it `sizeof(union) * drawableCount` so every entry sits at a fixed stride
    /// whatever variant it is. That is why a fill layer's buffer is 96 bytes per drawable when
    /// `FillDrawableUBO` is 80 — the pattern variants are larger, and they set the stride for
    /// everyone.
    ///
    /// Packing at the individual block's size instead would put every entry after the first at
    /// the wrong offset, and the symptom is a layer whose tiles are drawn with each other's
    /// matrices. The dump's sizes are what catch it:
    ///
    /// ```text
    /// ubo layer:0 slot=2 size=576    6 background drawables
    /// ubo layer:1 slot=2 size=1152   12 fill drawables (6 fills + 6 outlines)
    /// ubo layer:1 slot=4 size=576    12 fill tile props
    /// ubo layer:3 slot=2 size=768    6 line drawables
    /// ubo layer:3 slot=3 size=384    6 line tile props
    /// ```
    #[test]
    fn union_strides_explain_the_oracles_buffer_sizes() {
        use super::ubo_layouts::{
            BACKGROUND_DRAWABLE_UNION_UBO, FILL_DRAWABLE_UNION_UBO, FILL_TILE_PROPS_UNION_UBO,
            LINE_DRAWABLE_UNION_UBO, LINE_TILE_PROPS_UNION_UBO,
        };

        assert_eq!(BACKGROUND_DRAWABLE_UNION_UBO.stride * 6, 576);
        assert_eq!(FILL_DRAWABLE_UNION_UBO.stride * 12, 1152);
        assert_eq!(FILL_TILE_PROPS_UNION_UBO.stride * 12, 576);
        assert_eq!(LINE_DRAWABLE_UNION_UBO.stride * 6, 768);
        assert_eq!(LINE_TILE_PROPS_UNION_UBO.stride * 6, 384);
    }

    /// A union is as large as its largest member and no larger, and larger than the member a
    /// plain fill actually uses — which is the whole reason the stride is not the block size.
    #[test]
    fn a_union_is_its_largest_member() {
        use super::ubo_layouts::{FILL_DRAWABLE_UNION_UBO, LAYOUTS};

        let member_stride = |name: &str| {
            LAYOUTS
                .iter()
                .find(|layout| layout.name == name)
                .unwrap_or_else(|| panic!("{name} is a known block"))
                .stride
        };

        let largest = FILL_DRAWABLE_UNION_UBO
            .members
            .iter()
            .map(|name| member_stride(name))
            .max()
            .expect("members");
        assert_eq!(FILL_DRAWABLE_UNION_UBO.stride, largest);
    }

    /// The stride a plain fill is packed at is larger than the block it actually writes.
    ///
    /// Asserted at compile time rather than in a test body, because both sides are generated
    /// constants: if a future mbgl made the variants the same size, this would stop building
    /// rather than stop being checked. That is the right failure — the packing code reads the
    /// union stride precisely because it cannot assume the two agree.
    const _: () = assert!(
        super::ubo_layouts::FILL_DRAWABLE_UNION_UBO.stride
            > super::ubo_layouts::FILL_DRAWABLE_UBO.stride
    );

    /// Every union names blocks that exist, so a stride can always be recomputed from them.
    #[test]
    fn every_union_names_known_blocks() {
        use super::ubo_layouts::{LAYOUTS, UNIONS};

        for union in UNIONS {
            assert!(!union.members.is_empty(), "{}", union.name);
            for member in union.members {
                assert!(
                    LAYOUTS.iter().any(|layout| layout.name == *member),
                    "{} names {member}, which is not a known block",
                    union.name
                );
            }
        }
    }

    /// Every slot the oracle writes is reproduced by evaluating the header's enum chain.
    ///
    /// The dump's UBO section names a layer and a slot for each buffer:
    ///
    /// ```text
    /// ubo global:0 slot=0    GlobalPaintParamsUBO
    /// ubo layer:0  slot=2    BackgroundDrawableUBO
    /// ubo layer:0  slot=5    BackgroundPropsUBO
    /// ubo layer:1  slot=2    FillDrawableUBO
    /// ubo layer:1  slot=4    FillTilePropsUBO
    /// ubo layer:1  slot=5    FillEvaluatedPropsUBO
    /// ```
    ///
    /// None of those numbers is written down anywhere in mbgl. They come out of a chain of
    /// anonymous enums that take their values from each other, through a `std::max` over fifteen
    /// layer counts and a macro whose expansion depends on the render backend. Six independent
    /// agreements is what says the chain was evaluated rather than curve-fitted.
    #[test]
    fn the_slots_match_the_oracles_ubo_writes() {
        use super::ubo_slots::{
            ID_BACKGROUND_DRAWABLE_UBO, ID_BACKGROUND_PROPS_UBO, ID_FILL_DRAWABLE_UBO,
            ID_FILL_EVALUATED_PROPS_UBO, ID_FILL_TILE_PROPS_UBO, ID_GLOBAL_PAINT_PARAMS_UBO,
        };

        assert_eq!(ID_GLOBAL_PAINT_PARAMS_UBO, 0);
        assert_eq!(ID_BACKGROUND_DRAWABLE_UBO, 2);
        assert_eq!(ID_BACKGROUND_PROPS_UBO, 5);
        assert_eq!(ID_FILL_DRAWABLE_UBO, 2);
        assert_eq!(ID_FILL_TILE_PROPS_UBO, 4);
        assert_eq!(ID_FILL_EVALUATED_PROPS_UBO, 5);
    }

    /// The chain's structure holds, not just its endpoints.
    ///
    /// Layer slots start after the global ones, drawable slots sit inside the reserved range,
    /// and props slots sit at or above the shared start. Checking the shape as well as the
    /// values means a future mbgl that renumbered everything consistently would still be caught
    /// if it broke the ordering the packer depends on.
    ///
    /// Asserted at compile time, because every operand is a generated constant: a chain that
    /// came out inconsistent should stop the build rather than wait for a test run.
    const _: () = {
        use super::ubo_slots::{
            DRAWABLE_RESERVED_UBO_COUNT, GLOBAL_UBO_COUNT, ID_FILL_DRAWABLE_UBO,
            ID_FILL_EVALUATED_PROPS_UBO, LAYER_SSBO_START_ID, LAYER_UBO_START_ID,
        };

        assert!(LAYER_SSBO_START_ID == GLOBAL_UBO_COUNT);
        assert!(ID_FILL_DRAWABLE_UBO >= LAYER_SSBO_START_ID);
        assert!(ID_FILL_DRAWABLE_UBO < DRAWABLE_RESERVED_UBO_COUNT);
        assert!(LAYER_UBO_START_ID >= DRAWABLE_RESERVED_UBO_COUNT);
        assert!(ID_FILL_EVALUATED_PROPS_UBO >= LAYER_UBO_START_ID);
    };

    /// A drawable slot and a props slot within one layer are distinct, or one buffer would
    /// overwrite the other. Compile time, for the same reason as above.
    const _: () = {
        use super::ubo_slots::{
            ID_BACKGROUND_DRAWABLE_UBO, ID_BACKGROUND_PROPS_UBO, ID_FILL_DRAWABLE_UBO,
            ID_FILL_EVALUATED_PROPS_UBO, ID_FILL_TILE_PROPS_UBO,
        };

        assert!(ID_BACKGROUND_DRAWABLE_UBO != ID_BACKGROUND_PROPS_UBO);
        assert!(ID_FILL_DRAWABLE_UBO != ID_FILL_EVALUATED_PROPS_UBO);
        assert!(ID_FILL_TILE_PROPS_UBO != ID_FILL_EVALUATED_PROPS_UBO);
        assert!(ID_FILL_DRAWABLE_UBO != ID_FILL_TILE_PROPS_UBO);
    };

    /// The evaluation produced a whole table, not just the handful R0 reads.
    ///
    /// A chain that stopped early would still satisfy every assertion above, because those name
    /// only the symbols R0 needs — and would leave R1 to discover the gap.
    #[test]
    fn the_whole_chain_was_evaluated() {
        use super::ubo_slots::SLOTS;

        assert!(SLOTS.len() > 100, "{} symbols", SLOTS.len());
        for (name, _) in SLOTS {
            assert!(!name.is_empty());
        }
        for required in [
            "idLineDrawableUBO",
            "idSymbolDrawableUBO",
            "idCircleDrawableUBO",
            "idRasterDrawableUBO",
        ] {
            assert!(
                SLOTS.iter().any(|(name, _)| *name == required),
                "{required} is missing, so the chain stopped short of the later layers"
            );
        }
    }
}
