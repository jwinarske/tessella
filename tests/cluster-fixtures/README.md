# Clustering fixture

`places.json` is maplibre-native's own `test/fixtures/supercluster/places.json`: 162 populated
places from Natural Earth, which is what supercluster's test suite clusters. It is here for the
reason the MVT fixtures are — a port is checked against the implementation it came from, and
that needs the same input.

Provenance: it is part of maplibre-native's tree rather than of a vendored dependency's, so it
travels under the same terms as the rest of that repository. See `tests/mvt-fixtures/README.md`
for the rule this follows.

What it pins, in `tessella-source/tests/clustering.rs`, is supercluster's own expectations over
it: the tile at 0/0/0 has thirty-nine features standing for a hundred and ninety-six points,
cluster 1 has four children of six, seven and two points and a place called Bermuda Islands, five
cluster ids expand at particular zooms, and ten named leaves come back in a particular order from
an offset of five. Those numbers are properties of the whole construction — the tree layout, the
visit order, the tie-breaking — rather than of the radius, which is why they are worth having.
