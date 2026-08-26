//! Binary glTF, as a map's 3D buildings arrive in it.
//!
//! # What this is and is not
//!
//! glTF 2.0 is a Khronos specification and so are the two extensions a Mapbox buildings tile
//! requires — `KHR_mesh_quantization` and `EXT_meshopt_compression`. That matters for how this
//! was built: every rule the reader follows is written down in a published specification. What
//! *is* vendor-specific is one marker, `MAPBOX_mesh_features` in the asset's `extras`, and the
//! `mapbox:footprint:*` keys on each node — and neither changes how the file is read.
//!
//! # The two-buffer arrangement, which reads backwards
//!
//! A meshopt-compressed file declares **two** buffers. Buffer 0 is the GLB's binary chunk and
//! holds the compressed bytes. Buffer 1 is larger, has no data at all, and is marked
//! `EXT_meshopt_compression: { fallback: true }` — it is the *destination*, sized for the
//! decompressed result.
//!
//! Every `bufferView` then points at the fallback buffer for its offset and length, and carries
//! an `EXT_meshopt_compression` object saying where in buffer 0 its compressed bytes actually
//! are. So a reader that took the views at face value would read a buffer that does not exist,
//! and one that ignored the extension would read compressed bytes as vertices. The indirection
//! is the point of the extension: a viewer without it sees a valid file with an empty buffer,
//! and fails cleanly rather than drawing noise.
//!
//! # Bounds are checked here, not by the caller
//!
//! Every offset and length in a glTF comes from a JSON document that arrived over a network.
//! An accessor names a buffer view, a view names a buffer and a span of it, and each of those is
//! a number somebody else wrote. They are resolved and bounds-checked at parse rather than at
//! use, so a malformed file is one error rather than a read that happens to land inside another
//! mesh's vertices.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Why a glTF could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GltfError {
    /// The bytes do not begin with the GLB magic.
    #[error("not a binary glTF: the magic is missing")]
    NotGlb,
    /// A version this reader does not implement.
    ///
    /// glTF 1.0 is a different format rather than an earlier dialect of this one — different
    /// material model, different buffer layout — so it is refused by number instead of being
    /// attempted.
    #[error("glTF version {0} is not glTF 2.0")]
    Version(u32),
    /// The container ended inside a chunk.
    #[error("the file ends inside a chunk at byte {at}")]
    Truncated {
        /// Where it ran out.
        at: usize,
    },
    /// The JSON chunk is missing or unparseable.
    #[error("the glTF json did not parse: {0}")]
    Json(String),
    /// An extension the file marks as required and this build does not implement.
    ///
    /// Reported by name rather than ignored. A required extension is the file saying it cannot
    /// be read correctly without it — the geometry would be read as something else entirely —
    /// and glTF's own conformance rules say to refuse.
    #[error("this glTF requires `{0}`, which this build does not implement")]
    UnsupportedExtension(String),
    /// A reference points outside what it names.
    #[error("{what} {index} is out of range")]
    BadReference {
        /// What kind of thing was referenced.
        what: &'static str,
        /// The index given.
        index: usize,
    },
    /// A span runs past the end of its buffer.
    #[error("a {what} spans {offset}..{end} of a {length}-byte buffer")]
    OutOfBounds {
        /// What kind of span.
        what: &'static str,
        /// Where it starts.
        offset: usize,
        /// Where it would end.
        end: usize,
        /// How long the buffer is.
        length: usize,
    },
}

/// The extensions this reader implements.
///
/// A file requiring anything else is refused by name. `KHR_texture_transform` is *used* by a
/// buildings tile but not required, which is the distinction glTF draws: a viewer that ignores
/// it gets the texture at the wrong scale rather than a broken mesh, so the file does not insist.
pub const SUPPORTED: [&str; 2] = ["KHR_mesh_quantization", "EXT_meshopt_compression"];

/// How a meshopt-compressed view was packed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Interleaved vertex attributes.
    Attributes,
    /// Triangle indices, reordered for the vertex cache.
    Triangles,
    /// Index sequences, for primitives that are not triangle lists.
    Indices,
}

/// A reversible transform applied before compression.
///
/// Filters make a stream more compressible by moving it into a representation where neighbouring
/// values differ in fewer bits; they are undone after decoding. `Exponential` is what a buildings
/// tile uses for its positions — a shared exponent per component with an integer mantissa, which
/// keeps a building's coordinates to the precision they were quantised at rather than to the
/// precision a float would spend bits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    /// None; the bytes are the values.
    None,
    /// Octahedral, for unit vectors such as normals.
    Octahedral,
    /// Quaternion, for rotations.
    Quaternion,
    /// Exponential, for floating-point data.
    Exponential,
}

/// Where a buffer view's compressed bytes are, and how to read them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compressed {
    /// Which buffer holds them. Zero in every file seen, which is the GLB's binary chunk.
    pub buffer: usize,
    /// Where in that buffer.
    pub byte_offset: usize,
    /// How many compressed bytes.
    pub byte_length: usize,
    /// Bytes per element after decoding.
    pub byte_stride: usize,
    /// How it was packed.
    pub mode: Mode,
    /// What transform to undo.
    pub filter: Filter,
    /// How many elements decode out.
    pub count: usize,
}

/// One view onto a buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferView {
    /// Which buffer.
    pub buffer: usize,
    /// Where in it.
    pub byte_offset: usize,
    /// How long.
    pub byte_length: usize,
    /// Bytes between elements, when the view is interleaved.
    pub byte_stride: Option<usize>,
    /// Where the real bytes are, when this view is compressed.
    pub compressed: Option<Compressed>,
}

/// What a component of an accessor is.
///
/// The numbers are glTF's own, which are OpenGL's. Kept as the spec spells them because an
/// accessor states one and nothing else says what it means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentType {
    /// `5120`.
    Byte,
    /// `5121`.
    UnsignedByte,
    /// `5122`.
    Short,
    /// `5123`.
    UnsignedShort,
    /// `5125`.
    UnsignedInt,
    /// `5126`.
    Float,
}

impl ComponentType {
    /// Reads the spec's number.
    #[must_use]
    pub const fn from_code(code: u64) -> Option<Self> {
        match code {
            5120 => Some(Self::Byte),
            5121 => Some(Self::UnsignedByte),
            5122 => Some(Self::Short),
            5123 => Some(Self::UnsignedShort),
            5125 => Some(Self::UnsignedInt),
            5126 => Some(Self::Float),
            _ => None,
        }
    }

    /// How many bytes one component takes.
    #[must_use]
    pub const fn size(self) -> usize {
        match self {
            Self::Byte | Self::UnsignedByte => 1,
            Self::Short | Self::UnsignedShort => 2,
            Self::UnsignedInt | Self::Float => 4,
        }
    }
}

/// How many components an element has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementType {
    /// One.
    Scalar,
    /// Two.
    Vec2,
    /// Three.
    Vec3,
    /// Four.
    Vec4,
    /// Four, as a 2x2.
    Mat2,
    /// Nine.
    Mat3,
    /// Sixteen.
    Mat4,
}

impl ElementType {
    /// Reads the spec's name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "SCALAR" => Some(Self::Scalar),
            "VEC2" => Some(Self::Vec2),
            "VEC3" => Some(Self::Vec3),
            "VEC4" => Some(Self::Vec4),
            "MAT2" => Some(Self::Mat2),
            "MAT3" => Some(Self::Mat3),
            "MAT4" => Some(Self::Mat4),
            _ => None,
        }
    }

    /// How many components.
    #[must_use]
    pub const fn count(self) -> usize {
        match self {
            Self::Scalar => 1,
            Self::Vec2 => 2,
            Self::Vec3 | Self::Mat3 => 3,
            Self::Vec4 | Self::Mat2 => 4,
            Self::Mat4 => 16,
        }
    }
}

/// A typed window onto a buffer view.
#[derive(Debug, Clone, PartialEq)]
pub struct Accessor {
    /// Which view, if any. An accessor with none reads as zeros — glTF allows it for sparse data.
    pub buffer_view: Option<usize>,
    /// Where in the view.
    pub byte_offset: usize,
    /// What each component is.
    pub component_type: ComponentType,
    /// How many elements.
    pub count: usize,
    /// How many components per element.
    pub element_type: ElementType,
    /// Whether integers are to be read as a fraction of their range.
    ///
    /// `KHR_mesh_quantization`'s whole purpose: a normal stored as three signed bytes is
    /// `normalized`, and a reader ignoring the flag gets values in the hundreds where it wanted
    /// values around one.
    pub normalized: bool,
    /// The declared per-component minimum, when the file states one.
    pub min: Vec<f64>,
    /// The declared per-component maximum.
    pub max: Vec<f64>,
}

impl Accessor {
    /// Bytes one element occupies, unpacked.
    #[must_use]
    pub const fn element_size(&self) -> usize {
        self.component_type.size() * self.element_type.count()
    }
}

/// One drawable piece of a mesh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Primitive {
    /// Accessor index per attribute name, as the file spells them — `POSITION`, `NORMAL`, and so on.
    pub attributes: Vec<(String, usize)>,
    /// The index accessor, when the primitive is indexed.
    pub indices: Option<usize>,
    /// The material, when it names one.
    pub material: Option<usize>,
}

impl Primitive {
    /// The accessor for a named attribute.
    #[must_use]
    pub fn attribute(&self, name: &str) -> Option<usize> {
        self.attributes
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, index)| *index)
    }
}

/// A placed instance of a mesh.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// Column-major local transform, or the identity.
    ///
    /// A buildings tile puts the whole placement here — a scale into tile units and a translation
    /// to the footprint's corner — so a node's matrix is not decoration but where the building
    /// stands.
    pub matrix: [f64; 16],
    /// Which mesh, when it draws one.
    pub mesh: Option<usize>,
    /// Children, by index.
    pub children: Vec<usize>,
    /// Mapbox's footprint id, from `extras`.
    ///
    /// Proprietary and carried rather than interpreted: it is what ties a drawn building back to
    /// the feature a query would return, and nothing else in the file identifies one.
    pub footprint_id: Option<String>,
}

/// A parsed binary glTF.
#[derive(Debug, Clone, PartialEq)]
pub struct Gltf {
    /// Buffer lengths, in declaration order. A fallback buffer's is its *decompressed* size.
    pub buffers: Vec<usize>,
    /// Which buffers are meshopt fallbacks, and therefore hold no bytes of their own.
    pub fallback_buffers: Vec<bool>,
    /// The views.
    pub views: Vec<BufferView>,
    /// The accessors.
    pub accessors: Vec<Accessor>,
    /// Meshes, each a list of primitives.
    pub meshes: Vec<Vec<Primitive>>,
    /// Nodes.
    pub nodes: Vec<Node>,
    /// The binary chunk, which is buffer zero.
    pub binary: Vec<u8>,
    /// Whether the asset carries Mapbox's mesh-features marker.
    pub mapbox_mesh_features: bool,
}

/// The largest GLB this will read.
///
/// A buildings pack's largest entry seen is under a megabyte; sixty-four mebibytes is far past
/// any real one. It exists because every length below comes out of a document from the network.
pub const MAX_GLB_BYTES: usize = 64 * 1024 * 1024;

/// Reads a binary glTF.
///
/// # Errors
///
/// [`GltfError`] when the container is malformed, the version is not 2.0, a required extension
/// is not implemented, or a reference or span points outside what it names.
pub fn parse(bytes: &[u8]) -> Result<Gltf, GltfError> {
    if bytes.len() < 12 || &bytes[..4] != b"glTF" {
        return Err(GltfError::NotGlb);
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().map_err(|_| GltfError::NotGlb)?);
    if version != 2 {
        return Err(GltfError::Version(version));
    }
    // The header's total is advisory: a file that states more than it carries is truncated, and
    // one that states less has trailing bytes this ignores. The chunk walk below is what
    // actually bounds the read.
    let declared = u32::from_le_bytes(bytes[8..12].try_into().map_err(|_| GltfError::NotGlb)?);
    let end = (declared as usize).min(bytes.len()).min(MAX_GLB_BYTES);

    let mut json: Option<&[u8]> = None;
    let mut binary: Vec<u8> = Vec::new();
    let mut at = 12usize;
    while at + 8 <= end {
        let length = u32::from_le_bytes(
            bytes[at..at + 4]
                .try_into()
                .map_err(|_| GltfError::Truncated { at })?,
        ) as usize;
        let kind = &bytes[at + 4..at + 8];
        let start = at + 8;
        let stop = start
            .checked_add(length)
            .filter(|stop| *stop <= end)
            .ok_or(GltfError::Truncated { at: start })?;

        match kind {
            b"JSON" => json = Some(&bytes[start..stop]),
            b"BIN\0" => binary = bytes[start..stop].to_vec(),
            // An unknown chunk is skipped, which glTF requires: the format is extended by
            // adding chunks, and a reader that refused them could not read a later file that
            // is otherwise entirely readable.
            _ => {}
        }
        at = stop;
    }

    let json = json.ok_or_else(|| GltfError::Json("no JSON chunk".to_string()))?;
    let document: serde_json::Value =
        serde_json::from_slice(json).map_err(|error| GltfError::Json(error.to_string()))?;

    build(&document, binary)
}

/// Turns a parsed document into the model, checking every reference.
fn build(document: &serde_json::Value, binary: Vec<u8>) -> Result<Gltf, GltfError> {
    for required in document
        .get("extensionsRequired")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let name = required.as_str().unwrap_or_default();
        if !SUPPORTED.contains(&name) {
            return Err(GltfError::UnsupportedExtension(name.to_string()));
        }
    }

    let array = |key: &str| {
        document
            .get(key)
            .and_then(serde_json::Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
    };
    let number = |value: Option<&serde_json::Value>, fallback: usize| {
        value
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(fallback)
    };

    let mut buffers = Vec::new();
    let mut fallback_buffers = Vec::new();
    for buffer in array("buffers") {
        buffers.push(number(buffer.get("byteLength"), 0));
        fallback_buffers.push(
            buffer
                .pointer("/extensions/EXT_meshopt_compression/fallback")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        );
    }

    let mut views = Vec::new();
    for view in array("bufferViews") {
        let buffer = number(view.get("buffer"), usize::MAX);
        if buffer >= buffers.len() {
            return Err(GltfError::BadReference {
                what: "buffer",
                index: buffer,
            });
        }
        let byte_offset = number(view.get("byteOffset"), 0);
        let byte_length = number(view.get("byteLength"), 0);

        let compressed = view
            .pointer("/extensions/EXT_meshopt_compression")
            .map(|extension| {
                let source = number(extension.get("buffer"), usize::MAX);
                let offset = number(extension.get("byteOffset"), 0);
                let length = number(extension.get("byteLength"), 0);
                Ok::<_, GltfError>(Compressed {
                    buffer: source,
                    byte_offset: offset,
                    byte_length: length,
                    byte_stride: number(extension.get("byteStride"), 0),
                    mode: match extension.get("mode").and_then(serde_json::Value::as_str) {
                        Some("ATTRIBUTES") => Mode::Attributes,
                        Some("TRIANGLES") => Mode::Triangles,
                        Some("INDICES") => Mode::Indices,
                        other => {
                            return Err(GltfError::UnsupportedExtension(alloc::format!(
                                "EXT_meshopt_compression mode {}",
                                other.unwrap_or("(absent)")
                            )));
                        }
                    },
                    filter: match extension.get("filter").and_then(serde_json::Value::as_str) {
                        None | Some("NONE") => Filter::None,
                        Some("OCTAHEDRAL") => Filter::Octahedral,
                        Some("QUATERNION") => Filter::Quaternion,
                        Some("EXPONENTIAL") => Filter::Exponential,
                        Some(other) => {
                            return Err(GltfError::UnsupportedExtension(alloc::format!(
                                "EXT_meshopt_compression filter {other}"
                            )));
                        }
                    },
                    count: number(extension.get("count"), 0),
                })
            })
            .transpose()?;

        // A compressed view's own span addresses the *fallback* buffer, which holds nothing —
        // so only the compressed span is checked against real bytes.
        if let Some(compressed) = &compressed {
            let source = compressed.buffer.eq(&0).then_some(binary.len()).ok_or(
                GltfError::BadReference {
                    what: "compressed buffer",
                    index: compressed.buffer,
                },
            )?;
            let stop = compressed
                .byte_offset
                .checked_add(compressed.byte_length)
                .ok_or(GltfError::OutOfBounds {
                    what: "compressed view",
                    offset: compressed.byte_offset,
                    end: usize::MAX,
                    length: source,
                })?;
            if stop > source {
                return Err(GltfError::OutOfBounds {
                    what: "compressed view",
                    offset: compressed.byte_offset,
                    end: stop,
                    length: source,
                });
            }
        } else if !fallback_buffers[buffer] {
            // An uncompressed view of buffer zero reads the binary chunk directly.
            let source = if buffer == 0 {
                binary.len()
            } else {
                buffers[buffer]
            };
            let stop = byte_offset
                .checked_add(byte_length)
                .ok_or(GltfError::OutOfBounds {
                    what: "view",
                    offset: byte_offset,
                    end: usize::MAX,
                    length: source,
                })?;
            if stop > source {
                return Err(GltfError::OutOfBounds {
                    what: "view",
                    offset: byte_offset,
                    end: stop,
                    length: source,
                });
            }
        }

        views.push(BufferView {
            buffer,
            byte_offset,
            byte_length,
            byte_stride: view
                .get("byteStride")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| usize::try_from(value).ok()),
            compressed,
        });
    }

    let mut accessors = Vec::new();
    for accessor in array("accessors") {
        let buffer_view = accessor
            .get("bufferView")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| usize::try_from(value).ok());
        if let Some(index) = buffer_view
            && index >= views.len()
        {
            return Err(GltfError::BadReference {
                what: "buffer view",
                index,
            });
        }
        let component_type = ComponentType::from_code(
            number(accessor.get("componentType"), 0) as u64
        )
        .ok_or(GltfError::BadReference {
            what: "component type",
            index: number(accessor.get("componentType"), 0),
        })?;
        let element_type = accessor
            .get("type")
            .and_then(serde_json::Value::as_str)
            .and_then(ElementType::from_name)
            .ok_or(GltfError::BadReference {
                what: "element type",
                index: 0,
            })?;

        let numbers = |key: &str| -> Vec<f64> {
            accessor
                .get(key)
                .and_then(serde_json::Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(serde_json::Value::as_f64)
                        .collect()
                })
                .unwrap_or_default()
        };

        accessors.push(Accessor {
            buffer_view,
            byte_offset: number(accessor.get("byteOffset"), 0),
            component_type,
            count: number(accessor.get("count"), 0),
            element_type,
            normalized: accessor
                .get("normalized")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            min: numbers("min"),
            max: numbers("max"),
        });
    }

    let mut meshes = Vec::new();
    for mesh in array("meshes") {
        let mut primitives = Vec::new();
        for primitive in mesh
            .get("primitives")
            .and_then(serde_json::Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let mut attributes = Vec::new();
            if let Some(map) = primitive
                .get("attributes")
                .and_then(serde_json::Value::as_object)
            {
                for (name, index) in map {
                    let index = number(Some(index), usize::MAX);
                    if index >= accessors.len() {
                        return Err(GltfError::BadReference {
                            what: "attribute accessor",
                            index,
                        });
                    }
                    attributes.push((name.clone(), index));
                }
            }
            // Sorted, so two readings of one file produce the same order — a JSON object has
            // none, and a caller comparing primitives would otherwise see a difference that is
            // not in the file.
            attributes.sort_by(|a, b| a.0.cmp(&b.0));

            let indices = primitive
                .get("indices")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| usize::try_from(value).ok());
            if let Some(index) = indices
                && index >= accessors.len()
            {
                return Err(GltfError::BadReference {
                    what: "index accessor",
                    index,
                });
            }

            primitives.push(Primitive {
                attributes,
                indices,
                material: primitive
                    .get("material")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok()),
            });
        }
        meshes.push(primitives);
    }

    let node_count = array("nodes").len();
    let mut nodes = Vec::new();
    for node in array("nodes") {
        let mut matrix = [0.0f64; 16];
        matrix[0] = 1.0;
        matrix[5] = 1.0;
        matrix[10] = 1.0;
        matrix[15] = 1.0;
        if let Some(values) = node.get("matrix").and_then(serde_json::Value::as_array)
            && values.len() == 16
        {
            for (slot, value) in matrix.iter_mut().zip(values) {
                *slot = value.as_f64().unwrap_or(0.0);
            }
        }

        let mesh = node
            .get("mesh")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| usize::try_from(value).ok());
        if let Some(index) = mesh
            && index >= meshes.len()
        {
            return Err(GltfError::BadReference {
                what: "mesh",
                index,
            });
        }

        let mut children = Vec::new();
        for child in node
            .get("children")
            .and_then(serde_json::Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let index = number(Some(child), usize::MAX);
            if index >= node_count {
                return Err(GltfError::BadReference {
                    what: "child node",
                    index,
                });
            }
            children.push(index);
        }

        nodes.push(Node {
            matrix,
            mesh,
            children,
            footprint_id: node
                .pointer("/extras/mapbox:footprint:id")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string),
        });
    }

    Ok(Gltf {
        buffers,
        fallback_buffers,
        views,
        accessors,
        meshes,
        nodes,
        binary,
        mapbox_mesh_features: document
            .pointer("/asset/extras/MAPBOX_mesh_features")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

impl Gltf {
    /// The compressed bytes a view names, ready for a meshopt decoder.
    ///
    /// `None` for a view that is not compressed, which reads from its buffer directly.
    #[must_use]
    pub fn compressed_bytes(&self, view: usize) -> Option<&[u8]> {
        let view = self.views.get(view)?;
        let compressed = view.compressed.as_ref()?;
        self.binary
            .get(compressed.byte_offset..compressed.byte_offset + compressed.byte_length)
    }
}
