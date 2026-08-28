//! Point clustering, transcribed from `supercluster.hpp`.
//!
//! # What it does
//!
//! A GeoJSON source with `cluster: true` does not hand its points to the map as they are. It
//! builds one index per zoom level from the deepest upwards, and at each level any group of
//! points within `clusterRadius` screen pixels collapses into a single point carrying
//! `point_count`. Zooming in splits a cluster back into its parts; the index is built once and
//! every level is a lookup.
//!
//! # Why it is a transcription
//!
//! Because clustering is a *choice* among many valid ones, and two implementations that both
//! "group nearby points" disagree about which points end up together. The order the index
//! visits neighbours in decides which cluster absorbs a point — see [`crate::kdbush`] — so the
//! grouping is a property of the whole construction rather than of the radius. mbgl ships
//! supercluster and a style that renders correctly against it renders differently against
//! anything else, so this follows it line for line and is checked against its own expectations.
//!
//! # The id carries the zoom it was made at
//!
//! A cluster's id is `(index << 5) + (zoom + 1)`: the low five bits say which level built it and
//! the rest indexes that level's array. That is what lets `children` and `expansion_zoom` find a
//! cluster's parts from the id alone, with no map from ids to levels — and it is why the level
//! count is capped at 32 and the point count at 27 bits.
//!
//! # Not built: `clusterProperties`
//!
//! supercluster takes a map/reduce pair that accumulates arbitrary properties into a cluster —
//! summing a population field, say. The style spec spells it `clusterProperties`, and this build
//! does not parse it, so there is nothing to thread through and a hook here would have no
//! caller. A cluster carries `cluster`, `cluster_id`, `point_count` and
//! `point_count_abbreviated`, which is what supercluster produces without one.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString as _};
use alloc::vec::Vec;

use tessella_style::Value;

use crate::geojson::{GeoJsonFeature, Geometry};
use crate::kdbush::KdBush;

/// How a source clusters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Options {
    /// Shallowest zoom to build a level for.
    pub min_zoom: u8,
    /// Deepest zoom that still clusters. Past it the points are drawn as they are.
    pub max_zoom: u8,
    /// Cluster radius, in pixels of a tile of `extent`.
    pub radius: f64,
    /// The tile extent the radius is relative to.
    pub extent: f64,
    /// How many points must fall together before they become a cluster rather than staying
    /// separate points.
    pub min_points: usize,
}

impl Default for Options {
    /// supercluster's own defaults, which are also the style spec's.
    fn default() -> Self {
        Self {
            min_zoom: 0,
            max_zoom: 16,
            radius: 40.0,
            extent: 512.0,
            min_points: 2,
        }
    }
}

/// One point in a level: either an original feature or a cluster of them.
#[derive(Debug, Clone)]
struct Cluster {
    /// Position in Mercator units, both axes zero to one.
    pos: (f64, f64),
    /// How many original points it stands for. One means it is an original point.
    num_points: u32,
    /// Its id: an index into the source features when `num_points` is one, otherwise
    /// `(index << 5) + (zoom + 1)` naming the level that built it.
    id: u32,
    /// The cluster it was absorbed into at the next level up, or zero.
    parent_id: u32,
    /// Whether the level above has already accounted for it.
    visited: bool,
}

/// One level of the pyramid.
#[derive(Debug, Default)]
struct Level {
    tree: KdBush,
    clusters: Vec<Cluster>,
}

impl Level {
    /// The deepest level: every feature as its own point.
    fn from_features(features: &[GeoJsonFeature]) -> Self {
        let mut clusters = Vec::with_capacity(features.len());
        for (index, feature) in features.iter().enumerate() {
            let Geometry::Point(points) = &feature.geometry else {
                // Only points cluster. mbgl's source refuses the whole feature the same way, by
                // asking for a point and getting nothing.
                continue;
            };
            let Some(point) = points.first() else { continue };
            #[allow(clippy::cast_possible_truncation)]
            clusters.push(Cluster {
                pos: project(point[0], point[1]),
                num_points: 1,
                id: index as u32,
                parent_id: 0,
                visited: false,
            });
        }
        let tree = KdBush::new(&clusters.iter().map(|c| c.pos).collect::<Vec<_>>());
        Self { tree, clusters }
    }

    /// A level built by clustering the one below it.
    fn from_previous(previous: &mut Self, r: f64, zoom: u8, options: &Options) -> Self {
        let mut clusters: Vec<Cluster> = Vec::new();

        // The index is encoded in the upper 27 bits, so the count is clamped to what fits.
        let limit = previous.clusters.len().min(0x7ff_ffff);
        for index in 0..limit {
            if previous.clusters[index].visited {
                continue;
            }
            previous.clusters[index].visited = true;

            let origin = previous.clusters[index].pos;
            let num_points_origin = previous.clusters[index].num_points;

            // How many points a cluster here would hold, counting only those the levels above
            // have not already taken.
            let mut num_points = num_points_origin;
            let mut neighbours = Vec::new();
            previous.tree.within(origin.0, origin.1, r, &mut |id| {
                neighbours.push(id);
            });
            for &id in &neighbours {
                let neighbour = &previous.clusters[id as usize];
                if !neighbour.visited {
                    num_points += neighbour.num_points;
                }
            }

            if num_points as usize >= options.min_points {
                #[allow(clippy::cast_possible_truncation)]
                let id = ((index as u32) << 5) + u32::from(zoom) + 1;
                let mut weight = (
                    origin.0 * f64::from(num_points_origin),
                    origin.1 * f64::from(num_points_origin),
                );
                for &neighbour_id in &neighbours {
                    let neighbour = &mut previous.clusters[neighbour_id as usize];
                    if neighbour.visited {
                        continue;
                    }
                    neighbour.visited = true;
                    neighbour.parent_id = id;
                    weight.0 += neighbour.pos.0 * f64::from(neighbour.num_points);
                    weight.1 += neighbour.pos.1 * f64::from(neighbour.num_points);
                }
                previous.clusters[index].parent_id = id;
                clusters.push(Cluster {
                    pos: (
                        weight.0 / f64::from(num_points),
                        weight.1 / f64::from(num_points),
                    ),
                    num_points,
                    id,
                    parent_id: 0,
                    visited: false,
                });
            } else {
                // Too few to cluster: this point carries up as itself, and so does anything it
                // reached — which is why they are marked visited here rather than left for the
                // next iteration to find and cluster differently.
                clusters.push(Cluster {
                    pos: origin,
                    num_points: 1,
                    id: previous.clusters[index].id,
                    parent_id: 0,
                    visited: false,
                });
                if num_points > 1 {
                    for &neighbour_id in &neighbours {
                        let neighbour = &mut previous.clusters[neighbour_id as usize];
                        if neighbour.visited {
                            continue;
                        }
                        neighbour.visited = true;
                        let (pos, id) = (neighbour.pos, neighbour.id);
                        clusters.push(Cluster {
                            pos,
                            num_points: 1,
                            id,
                            parent_id: 0,
                            visited: false,
                        });
                    }
                }
            }
        }

        let tree = KdBush::new(&clusters.iter().map(|c| c.pos).collect::<Vec<_>>());
        Self { tree, clusters }
    }
}

/// A clustered point source: one index per zoom, built once.
#[derive(Debug)]
pub struct Clustered {
    features: Vec<GeoJsonFeature>,
    options: Options,
    /// Levels by zoom, from `min_zoom` to `max_zoom + 1`.
    levels: BTreeMap<u8, Level>,
}

/// A point as a tile carries it: tile-local coordinates and the properties to draw it with.
#[derive(Debug, Clone, PartialEq)]
pub struct TileFeature {
    /// Tile-local position, in units of the source's extent.
    pub position: (i16, i16),
    /// The cluster's properties, or the original feature's.
    pub properties: BTreeMap<String, Value>,
    /// The cluster's id, or the original feature's own.
    pub id: Option<Value>,
}

impl Clustered {
    /// Builds every level, deepest first.
    #[must_use]
    pub fn new(features: Vec<GeoJsonFeature>, options: Options) -> Self {
        let mut levels = BTreeMap::new();
        levels.insert(options.max_zoom + 1, Level::from_features(&features));

        for zoom in (options.min_zoom..=options.max_zoom).rev() {
            let radius = options.radius / (options.extent * libm::pow(2.0, f64::from(zoom)));
            let mut previous = levels.remove(&(zoom + 1)).expect("the level below");
            let level = Level::from_previous(&mut previous, radius, zoom, &options);
            levels.insert(zoom + 1, previous);
            levels.insert(zoom, level);
        }

        Self {
            features,
            options,
            levels,
        }
    }

    /// The points a tile draws, clustered for its zoom.
    #[must_use]
    pub fn tile(&self, z: u8, x: u32, y: u32) -> Vec<TileFeature> {
        let Some(level) = self.levels.get(&self.limit_zoom(z)) else {
            return Vec::new();
        };

        let z2 = libm::pow(2.0, f64::from(z));
        let r = self.options.radius / self.options.extent;
        let (x, y) = (f64::from(x), f64::from(y));

        let mut found = Vec::new();
        let top = (y - r) / z2;
        let bottom = (y + 1.0 + r) / z2;
        level.tree.range((x - r) / z2, top, (x + 1.0 + r) / z2, bottom, &mut |id| {
            found.push((id, x));
        });

        // A tile at either edge of the world also draws what wraps into it from the other side,
        // shifted by a world width so the tile-local coordinates come out on the near side.
        if x == 0.0 {
            level
                .tree
                .range(1.0 - r / z2, top, 1.0, bottom, &mut |id| found.push((id, z2)));
        }
        if (x - (z2 - 1.0)).abs() < f64::EPSILON {
            level
                .tree
                .range(0.0, top, r / z2, bottom, &mut |id| found.push((id, -1.0)));
        }

        found
            .into_iter()
            .map(|(id, origin_x)| {
                let cluster = &level.clusters[id as usize];
                #[allow(clippy::cast_possible_truncation)]
                let position = (
                    libm::round(self.options.extent * (cluster.pos.0 * z2 - origin_x)) as i16,
                    libm::round(self.options.extent * (cluster.pos.1 * z2 - y)) as i16,
                );
                if cluster.num_points == 1 {
                    let original = &self.features[cluster.id as usize];
                    TileFeature {
                        position,
                        properties: original.properties.clone(),
                        id: original.id.clone(),
                    }
                } else {
                    TileFeature {
                        position,
                        properties: cluster_properties(cluster),
                        id: Some(Value::Number(f64::from(cluster.id))),
                    }
                }
            })
            .collect()
    }

    /// The clusters and points one cluster splits into at the next level down.
    ///
    /// # Errors
    ///
    /// [`NoSuchCluster`] when the id names no level, no cluster in it, or a cluster with no
    /// children — which is what supercluster throws for.
    pub fn children(&self, cluster_id: u32) -> Result<Vec<GeoJsonFeature>, NoSuchCluster> {
        let mut out = Vec::new();
        self.each_child(cluster_id, &mut |cluster| {
            out.push(self.to_feature(cluster));
        })?;
        Ok(out)
    }

    /// The original points under a cluster, `limit` of them from `offset`.
    ///
    /// # Errors
    ///
    /// As [`Self::children`].
    pub fn leaves(
        &self,
        cluster_id: u32,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<GeoJsonFeature>, NoSuchCluster> {
        let mut out = Vec::new();
        let mut remaining = limit;
        let mut skipped = 0;
        self.each_leaf(cluster_id, &mut remaining, offset, &mut skipped, &mut out)?;
        Ok(out)
    }

    /// The zoom at which a cluster stops being one.
    ///
    /// Walking up while a cluster has exactly one child: a cluster that splits into one thing is
    /// the same cluster under another id, so the zoom that matters is the first that splits it
    /// into more.
    ///
    /// # Errors
    ///
    /// As [`Self::children`].
    pub fn expansion_zoom(&self, cluster_id: u32) -> Result<u8, NoSuchCluster> {
        let mut cluster_id = cluster_id;
        #[allow(clippy::cast_possible_truncation)]
        let mut zoom = ((cluster_id % 32) as u8).saturating_sub(1);
        while zoom <= self.options.max_zoom {
            let mut children = 0;
            let mut only = cluster_id;
            self.each_child(cluster_id, &mut |cluster| {
                children += 1;
                only = cluster.id;
            })?;
            cluster_id = only;
            zoom += 1;
            if children != 1 {
                break;
            }
        }
        Ok(zoom)
    }

    fn limit_zoom(&self, z: u8) -> u8 {
        z.clamp(self.options.min_zoom, self.options.max_zoom + 1)
    }

    fn each_child(
        &self,
        cluster_id: u32,
        visit: &mut dyn FnMut(&Cluster),
    ) -> Result<(), NoSuchCluster> {
        let origin_id = (cluster_id >> 5) as usize;
        #[allow(clippy::cast_possible_truncation)]
        let origin_zoom = (cluster_id % 32) as u8;

        let level = self.levels.get(&origin_zoom).ok_or(NoSuchCluster)?;
        let origin = level.clusters.get(origin_id).ok_or(NoSuchCluster)?;

        let r = self.options.radius
            / (self.options.extent * libm::pow(2.0, f64::from(origin_zoom) - 1.0));

        let mut ids = Vec::new();
        level
            .tree
            .within(origin.pos.0, origin.pos.1, r, &mut |id| ids.push(id));

        let mut found = false;
        for id in ids {
            let child = &level.clusters[id as usize];
            if child.parent_id == cluster_id {
                visit(child);
                found = true;
            }
        }
        if found { Ok(()) } else { Err(NoSuchCluster) }
    }

    fn each_leaf(
        &self,
        cluster_id: u32,
        limit: &mut u32,
        offset: u32,
        skipped: &mut u32,
        out: &mut Vec<GeoJsonFeature>,
    ) -> Result<(), NoSuchCluster> {
        let mut children = Vec::new();
        self.each_child(cluster_id, &mut |cluster| children.push(cluster.clone()))?;

        for child in children {
            if *limit == 0 {
                return Ok(());
            }
            if child.num_points > 1 {
                if *skipped + child.num_points <= offset {
                    *skipped += child.num_points;
                } else {
                    self.each_leaf(child.id, limit, offset, skipped, out)?;
                }
            } else if *skipped < offset {
                *skipped += 1;
            } else {
                out.push(self.to_feature(&child));
                *limit -= 1;
            }
        }
        Ok(())
    }

    /// A cluster as GeoJSON: the original feature for a single point, an invented one otherwise.
    fn to_feature(&self, cluster: &Cluster) -> GeoJsonFeature {
        if cluster.num_points == 1 {
            return self.features[cluster.id as usize].clone();
        }
        GeoJsonFeature {
            id: Some(Value::Number(f64::from(cluster.id))),
            properties: cluster_properties(cluster),
            geometry: Geometry::Point(alloc::vec![unproject(cluster.pos)]),
        }
    }
}

/// A cluster id that names nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("no cluster with that id")]
pub struct NoSuchCluster;

/// The properties supercluster gives a cluster.
fn cluster_properties(cluster: &Cluster) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    out.insert("cluster".to_string(), Value::Bool(true));
    out.insert(
        "cluster_id".to_string(),
        Value::Number(f64::from(cluster.id)),
    );
    out.insert(
        "point_count".to_string(),
        Value::Number(f64::from(cluster.num_points)),
    );
    out.insert(
        "point_count_abbreviated".to_string(),
        Value::String(abbreviate(cluster.num_points)),
    );
    out
}

/// supercluster's `point_count_abbreviated`, which a style draws as the cluster's label.
///
/// Under a thousand it is the number. Between one and ten thousand it is one decimal place and a
/// `k`; past that it is a whole number and a `k`, because a stream formatted with `std::fixed`
/// and no precision gives six decimals and the branch above it sets one.
fn abbreviate(count: u32) -> String {
    if count < 1000 {
        return alloc::format!("{count}");
    }
    let thousands = f64::from(count) / 1000.0;
    if count < 10_000 {
        alloc::format!("{thousands:.1}k")
    } else {
        alloc::format!("{thousands:.0}k")
    }
}

/// Longitude and latitude to Mercator units, both axes zero to one.
fn project(lng: f64, lat: f64) -> (f64, f64) {
    let x = lng / 360.0 + 0.5;
    let sine = libm::sin(lat * core::f64::consts::PI / 180.0);
    let y = 0.5 - 0.25 * libm::log((1.0 + sine) / (1.0 - sine)) / core::f64::consts::PI;
    (x, y.clamp(0.0, 1.0))
}

/// And back, for a cluster that has to be reported as a place on the earth.
fn unproject(pos: (f64, f64)) -> [f64; 2] {
    let lng = (pos.0 - 0.5) * 360.0;
    let lat = 360.0
        * libm::atan(libm::exp((180.0 - pos.1 * 360.0) * core::f64::consts::PI / 180.0))
        / core::f64::consts::PI
        - 90.0;
    [lng, lat]
}
