//! Generated mirrors of mbgl C++ types (DR-6).
//!
//! Nothing in here is written by hand. Each file records the maplibre-native revision it came
//! from in its header; `mbgl-codegen` rewrites them all from the pinned tree.

pub mod mbgl_enums;
pub mod shader_attributes;

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
}
