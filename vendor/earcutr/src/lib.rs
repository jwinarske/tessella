use itertools::Itertools;
use std::{iter, ops};

static DIM: usize = 2;
static NULL: usize = 0;

#[doc(hidden)]
pub mod legacy;

pub use legacy::deviation;
pub use legacy::flatten;

type LinkedListNodeIndex = usize;
type VerticesIndex = usize;

pub trait Float: num_traits::float::Float {}

impl<T> Float for T where T: num_traits::float::Float {}

#[derive(Debug, PartialEq, Copy, Clone)]
#[non_exhaustive]
pub enum Error {
    Unknown,
}

impl std::fmt::Display for Error {
    fn fmt(&self, mut f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Unknown => write!(&mut f, "Unknown error"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Coord<T: Float> {
    x: T,
    y: T,
}

#[derive(Clone, Copy, Debug)]
struct LinkedListNode<T: Float> {
    /// vertex index in flat one-d array of 64bit float coords
    vertices_index: VerticesIndex,
    /// vertex
    coord: Coord<T>,
    /// previous vertex node in a polygon ring
    prev_linked_list_node_index: LinkedListNodeIndex,
    /// next vertex node in a polygon ring
    next_linked_list_node_index: LinkedListNodeIndex,
    /// z-order curve value
    z: i32,
    /// previous node in z-order
    prevz_idx: LinkedListNodeIndex,
    /// next node in z-order
    nextz_idx: LinkedListNodeIndex,
    /// indicates whether this is a steiner point
    is_steiner_point: bool,
    /// index within LinkedLists vector that holds all nodes
    idx: LinkedListNodeIndex,
}

impl<T: Float> LinkedListNode<T> {
    fn new(i: VerticesIndex, coord: Coord<T>, idx: LinkedListNodeIndex) -> LinkedListNode<T> {
        LinkedListNode {
            vertices_index: i,
            coord,
            prev_linked_list_node_index: NULL,
            next_linked_list_node_index: NULL,
            z: 0,
            nextz_idx: NULL,
            prevz_idx: NULL,
            is_steiner_point: false,
            idx,
        }
    }

    // check if two points are equal
    fn xy_eq(&self, other: LinkedListNode<T>) -> bool {
        self.coord == other.coord
    }

    fn prev_linked_list_node(&self, linked_list_nodes: &LinkedLists<T>) -> LinkedListNode<T> {
        linked_list_nodes.nodes[self.prev_linked_list_node_index]
    }

    fn next_linked_list_node(&self, linked_list_nodes: &LinkedLists<T>) -> LinkedListNode<T> {
        linked_list_nodes.nodes[self.next_linked_list_node_index]
    }
}

pub struct LinkedLists<T: Float> {
    nodes: Vec<LinkedListNode<T>>,
    invsize: T,
    min: Coord<T>,
    max: Coord<T>,
    usehash: bool,
}

struct Vertices<'a, T: Float>(&'a [T]);

impl<'a, T: Float> Vertices<'a, T> {
    fn is_empty(&'a self) -> bool {
        self.0.is_empty()
    }

    fn len(&'a self) -> usize {
        self.0.len()
    }

    // tessella: each point paired with the one before it, cyclically from the last, and the terms
    // summed in point order -- what upstream's `cycle().skip().step_by()` chain did, as a loop.
    // The same terms in the same order, so the same sum to the bit. `start` and `end` bound whole
    // points: `earcut` refuses an odd-length array and hole starts are point indices.
    fn signed_area(&self, start: VerticesIndex, end: VerticesIndex) -> T {
        let v = self.0;
        let mut sum = T::zero();
        let mut j = end - DIM;
        let mut i = start;
        while i < end {
            sum = sum + (v[j] - v[i]) * (v[i + 1] + v[j + 1]);
            j = i;
            i += DIM;
        }
        sum
    }
}

// Note: none of the following macros work for Left-Hand-Side of assignment.
macro_rules! next {
    ($ll:expr,$idx:expr) => {
        $ll.nodes[$ll.nodes[$idx].next_linked_list_node_index]
    };
}
macro_rules! nextref {
    ($ll:expr,$idx:expr) => {
        &$ll.nodes[$ll.nodes[$idx].next_linked_list_node_index]
    };
}
macro_rules! prev {
    ($ll:expr,$idx:expr) => {
        $ll.nodes[$ll.nodes[$idx].prev_linked_list_node_index]
    };
}
macro_rules! prevref {
    ($ll:expr,$idx:expr) => {
        &$ll.nodes[$ll.nodes[$idx].prev_linked_list_node_index]
    };
}

impl<T: Float> LinkedLists<T> {
    // z-order of a point given coords and size of the data bounding box
    //
    // tessella: `earcut.hpp`'s `zOrder`, expression for expression -- `32767 * (x - min) * inv_size`
    // on the untranslated coordinate, truncated to a 32-bit integer, and each coordinate's bits
    // spread on its own. Upstream translated every point by the minimum up front and scaled by
    // `32767 / size`, which rounds differently. See PATCH.md.
    #[inline(always)]
    fn zorder(&self, x: T, y: T) -> i32 {
        let scale = num_traits::cast::<f64, T>(32767.0).unwrap();
        let quantize = |v: T, min: T| -> i32 {
            num_traits::cast::<T, f64>(scale * (v - min) * self.invsize).map_or(0, |v| v as i32)
        };
        let mut x = quantize(x, self.min.x);
        let mut y = quantize(y, self.min.y);

        x = (x | (x << 8)) & 0x00FF00FF;
        x = (x | (x << 4)) & 0x0F0F0F0F;
        x = (x | (x << 2)) & 0x33333333;
        x = (x | (x << 1)) & 0x55555555;

        y = (y | (y << 8)) & 0x00FF00FF;
        y = (y | (y << 4)) & 0x0F0F0F0F;
        y = (y | (y << 2)) & 0x33333333;
        y = (y | (y << 1)) & 0x55555555;

        x | (y << 1)
    }

    fn iter_pairs(&self, r: ops::Range<LinkedListNodeIndex>) -> NodePairIterator<T> {
        NodePairIterator::new(self, r.start, r.end)
    }

    fn insert_node(
        &mut self,
        i: VerticesIndex,
        coord: Coord<T>,
        last: Option<LinkedListNodeIndex>,
    ) -> LinkedListNodeIndex {
        let mut p = LinkedListNode::new(i, coord, self.nodes.len());
        match last {
            None => {
                p.next_linked_list_node_index = p.idx;
                p.prev_linked_list_node_index = p.idx;
            }
            Some(last) => {
                p.next_linked_list_node_index = self.nodes[last].next_linked_list_node_index;
                p.prev_linked_list_node_index = last;
                let lastnextidx = self.nodes[last].next_linked_list_node_index;
                self.nodes[lastnextidx].prev_linked_list_node_index = p.idx;
                self.nodes[last].next_linked_list_node_index = p.idx;
            }
        }
        let result = p.idx;
        self.nodes.push(p);
        result
    }
    fn remove_node(&mut self, p_idx: LinkedListNodeIndex) {
        let pi = self.nodes[p_idx].prev_linked_list_node_index;
        let ni = self.nodes[p_idx].next_linked_list_node_index;
        let pz = self.nodes[p_idx].prevz_idx;
        let nz = self.nodes[p_idx].nextz_idx;
        self.nodes[pi].next_linked_list_node_index = ni;
        self.nodes[ni].prev_linked_list_node_index = pi;
        self.nodes[pz].nextz_idx = nz;
        self.nodes[nz].prevz_idx = pz;
    }
    fn new(size_hint: usize) -> LinkedLists<T> {
        let mut ll = LinkedLists {
            nodes: Vec::with_capacity(size_hint),
            invsize: T::zero(),
            min: Coord {
                x: T::max_value(),
                y: T::max_value(),
            },
            max: Coord {
                x: T::min_value(),
                y: T::min_value(),
            },
            usehash: true,
        };
        // ll.nodes[0] is the NULL node. For example usage, see remove_node()
        ll.nodes.push(LinkedListNode {
            vertices_index: 0,
            coord: Coord {
                x: T::zero(),
                y: T::zero(),
            },
            prev_linked_list_node_index: 0,
            next_linked_list_node_index: 0,
            z: 0,
            nextz_idx: 0,
            prevz_idx: 0,
            is_steiner_point: false,
            idx: 0,
        });
        ll
    }

    // interlink polygon nodes in z-order
    fn index_curve(&mut self, start: LinkedListNodeIndex) -> Result<(), Error> {
        let mut p = start;
        loop {
            if self.nodes[p].z == 0 {
                let coord = self.nodes[p].coord;
                self.nodes[p].z = self.zorder(coord.x, coord.y);
            }
            self.nodes[p].prevz_idx = self.nodes[p].prev_linked_list_node_index;
            self.nodes[p].nextz_idx = self.nodes[p].next_linked_list_node_index;
            p = self.nodes[p].next_linked_list_node_index;
            if p == start {
                break;
            }
        }

        let pzi = self.nodes[start].prevz_idx;
        self.nodes[pzi].nextz_idx = NULL;
        self.nodes[start].prevz_idx = NULL;
        self.sort_linked(start);
        Ok(())
    }

    // find a bridge between vertices that connects hole with an outer ring
    // and and link it
    fn eliminate_hole(
        &mut self,
        hole_idx: LinkedListNodeIndex,
        outer_node_idx: LinkedListNodeIndex,
    ) {
        let test_idx = find_hole_bridge(self, hole_idx, outer_node_idx);
        // tessella: no bridge, no splice. `find_hole_bridge` answers NULL when no segment of the
        // outer ring lies to the hole's left, and splicing the NULL node in anyway links the
        // sentinel into the rings, which `filter_points` then walks for ever. `earcut.hpp` leaves
        // such a hole out -- `if (outerNode)` -- and so does this. See PATCH.md.
        if test_idx == NULL {
            return;
        }
        let b = split_bridge_polygon(self, test_idx, hole_idx);
        let ni = self.nodes[b].next_linked_list_node_index;
        filter_points(self, b, Some(ni));
    }

    // Simon Tatham's linked list merge sort algorithm
    // http://www.chiark.greenend.org.uk/~sgtatham/algorithms/listsort.html
    fn sort_linked(&mut self, mut list: LinkedListNodeIndex) {
        let mut p;
        let mut q;
        let mut e;
        let mut nummerges;
        let mut psize;
        let mut qsize;
        let mut insize = 1;
        let mut tail;

        loop {
            p = list;
            list = NULL;
            tail = NULL;
            nummerges = 0;

            while p != NULL {
                nummerges += 1;
                q = p;
                psize = 0;
                while q != NULL && psize < insize {
                    psize += 1;
                    q = self.nodes[q].nextz_idx;
                }
                qsize = insize;

                while psize > 0 || (qsize > 0 && q != NULL) {
                    if psize > 0 && (qsize == 0 || q == NULL || self.nodes[p].z <= self.nodes[q].z)
                    {
                        e = p;
                        p = self.nodes[p].nextz_idx;
                        psize -= 1;
                    } else {
                        e = q;
                        q = self.nodes[q].nextz_idx;
                        qsize -= 1;
                    }

                    if tail != NULL {
                        self.nodes[tail].nextz_idx = e;
                    } else {
                        list = e;
                    }

                    self.nodes[e].prevz_idx = tail;
                    tail = e;
                }

                p = q;
            }

            self.nodes[tail].nextz_idx = NULL;
            insize *= 2;
            if nummerges <= 1 {
                break;
            }
        }
    }

    // add new nodes to an existing linked list.
    fn add_contour(
        &mut self,
        vertices: &Vertices<T>,
        start: VerticesIndex,
        end: VerticesIndex,
        clockwise: bool,
    ) -> Result<LinkedListNodeIndex, Error> {
        if start > vertices.len() || end > vertices.len() || vertices.is_empty() {
            return Err(Error::Unknown);
        }

        if end < DIM || end - DIM < start {
            return Err(Error::Unknown);
        }

        if end < DIM {
            return Err(Error::Unknown);
        }

        // tessella: the ring and nothing else. Upstream also kept a bounding box here, for the
        // z-order hash, reading hole coordinates at ring-relative indices into the whole array;
        // `earcut.hpp` takes the box from the outer ring once the holes are bridged into it, and
        // so does `earcut` now. The hole's leftmost point is `get_leftmost`'s. See PATCH.md.
        let mut lastidx = None;

        let clockwise_iter = vertices.0[start..end].iter().copied().enumerate();

        let mut insert = |x_index: usize, x: T, y: T| {
            lastidx = Some(self.insert_node((start + x_index) / DIM, Coord { x, y }, lastidx));
        };

        if clockwise == (vertices.signed_area(start, end) > T::zero()) {
            for ((x_index, x), (_, y)) in clockwise_iter.tuples() {
                insert(x_index, x, y);
            }
        } else {
            for ((_, y), (x_index, x)) in clockwise_iter.rev().tuples() {
                insert(x_index, x, y);
            }
        }

        if self.nodes[lastidx.unwrap()].xy_eq(*nextref!(self, lastidx.unwrap())) {
            self.remove_node(lastidx.unwrap());
            lastidx = Some(self.nodes[lastidx.unwrap()].next_linked_list_node_index);
        }
        Ok(lastidx.unwrap())
    }

    // check if a diagonal between two polygon nodes is valid (lies in
    // polygon interior)
    //
    // tessella: `earcut.hpp`'s `isValidDiagonal`, which also refuses a diagonal that creates
    // opposite-facing sectors and accepts the zero-length one between two coincident convex
    // vertices. Upstream had neither. See PATCH.md.
    fn is_valid_diagonal(&self, a: &LinkedListNode<T>, b: &LinkedListNode<T>) -> bool {
        let zero = T::zero();
        let (ap, an) = (prev!(self, a.idx), next!(self, a.idx));
        let (bp, bn) = (prev!(self, b.idx), next!(self, b.idx));
        an.vertices_index != b.vertices_index
            && ap.vertices_index != b.vertices_index
            && !intersects_polygon(self, *a, *b)
            && ((locally_inside(self, a, b)
                && locally_inside(self, b, a)
                && middle_inside(self, a, b)
                && (coord_area(ap.coord, a.coord, bp.coord) != zero
                    || coord_area(a.coord, bp.coord, b.coord) != zero))
                || (a.xy_eq(*b)
                    && coord_area(ap.coord, a.coord, an.coord) > zero
                    && coord_area(bp.coord, b.coord, bn.coord) > zero))
    }
}

struct NodePairIterator<'a, T: Float> {
    cur: LinkedListNodeIndex,
    end: LinkedListNodeIndex,
    ll: &'a LinkedLists<T>,
    pending_result: Option<(&'a LinkedListNode<T>, &'a LinkedListNode<T>)>,
}

impl<'a, T: Float> NodePairIterator<'a, T> {
    fn new(
        ll: &LinkedLists<T>,
        start: LinkedListNodeIndex,
        end: LinkedListNodeIndex,
    ) -> NodePairIterator<T> {
        NodePairIterator {
            pending_result: Some((&ll.nodes[start], nextref!(ll, start))),
            cur: start,
            end,
            ll,
        }
    }
}

impl<'a, T: Float> Iterator for NodePairIterator<'a, T> {
    type Item = (&'a LinkedListNode<T>, &'a LinkedListNode<T>);
    fn next(&mut self) -> Option<Self::Item> {
        self.cur = self.ll.nodes[self.cur].next_linked_list_node_index;
        let cur_result = self.pending_result;
        if self.cur == self.end {
            // only one branch, saves time
            self.pending_result = None;
        } else {
            self.pending_result = Some((&self.ll.nodes[self.cur], nextref!(self.ll, self.cur)));
        }
        cur_result
    }
}

// link every hole into the outer loop, producing a single-ring polygon
// without holes
fn eliminate_holes<T: Float>(
    ll: &mut LinkedLists<T>,
    vertices: &Vertices<T>,
    hole_indices: &[VerticesIndex],
    inouter_node: LinkedListNodeIndex,
) -> Result<LinkedListNodeIndex, Error> {
    let mut outer_node = inouter_node;
    let mut queue: Vec<LinkedListNode<T>> = Vec::new();
    for (vertices_hole_start_index, vertices_hole_end_index) in hole_indices
        .iter()
        .map(|index| index.checked_mul(DIM).ok_or(Error::Unknown))
        .chain(iter::once(Ok(vertices.0.len())))
        .tuple_windows()
    {
        let vertices_hole_start_index = vertices_hole_start_index?;
        let vertices_hole_end_index = vertices_hole_end_index?;
        // tessella: a ring with no points is `linkedList`'s null, which `earcut.hpp` skips.
        // Upstream refused the whole polygon for it.
        if vertices_hole_start_index == vertices_hole_end_index {
            continue;
        }
        let list = ll.add_contour(
            vertices,
            vertices_hole_start_index,
            vertices_hole_end_index,
            false,
        )?;
        if list == ll.nodes[list].next_linked_list_node_index {
            ll.nodes[list].is_steiner_point = true;
        }
        queue.push(ll.nodes[get_leftmost(ll, list)]);
    }

    // tessella: in the order `earcut.hpp`'s `std::sort` leaves them, which is not the order a
    // stable sort leaves them in when two holes' leftmost points share an x. See `cpp_sort`.
    cpp_sort(
        &mut queue,
        &|a: &LinkedListNode<T>, b: &LinkedListNode<T>| a.coord.x < b.coord.x,
    );

    // process holes from left to right
    for node in queue {
        ll.eliminate_hole(node.idx, outer_node);
        let nextidx = next!(ll, outer_node).idx;
        outer_node = filter_points(ll, outer_node, Some(nextidx));
    }
    Ok(outer_node)
} // elim holes

// `std::sort` as libstdc++ implements it, for the one call `earcut.hpp` makes.
//
// tessella: `eliminateHoles` sorts the holes by the x of their leftmost points with `std::sort`,
// which is not stable, so holes whose points share an x come out in whatever order the library's
// sort leaves them -- and the order holes are bridged in is the triangulation. The oracle is
// built against libstdc++, whose sort is an introsort: median-of-three quicksort down to runs of
// sixteen, heapsort if it recurses too deep, then one insertion sort over the whole. Up to sixteen
// holes that is a plain insertion sort, and stable; above it, ties land where the partitioning
// puts them. This is that algorithm, step for step, so they land in the same places. See PATCH.md.
const CPP_SORT_THRESHOLD: usize = 16;

fn cpp_sort<E: Copy>(v: &mut [E], less: &impl Fn(&E, &E) -> bool) {
    let n = v.len();
    if n < 2 {
        return;
    }
    // `std::__lg(n) * 2`.
    let depth = 2 * (usize::BITS - 1 - n.leading_zeros()) as usize;
    cpp_introsort_loop(v, 0, n, depth, less);
    cpp_final_insertion_sort(v, 0, n, less);
}

fn cpp_introsort_loop<E: Copy>(
    v: &mut [E],
    first: usize,
    mut last: usize,
    mut depth: usize,
    less: &impl Fn(&E, &E) -> bool,
) {
    while last - first > CPP_SORT_THRESHOLD {
        if depth == 0 {
            // `std::__partial_sort(first, last, last)`: a heap of the whole range, then sorted.
            cpp_make_heap(&mut v[first..last], less);
            cpp_sort_heap(&mut v[first..last], less);
            return;
        }
        depth -= 1;
        let cut = cpp_unguarded_partition_pivot(v, first, last, less);
        cpp_introsort_loop(v, cut, last, depth, less);
        last = cut;
    }
}

fn cpp_unguarded_partition_pivot<E: Copy>(
    v: &mut [E],
    first: usize,
    last: usize,
    less: &impl Fn(&E, &E) -> bool,
) -> usize {
    let mid = first + (last - first) / 2;
    // `std::__move_median_to_first(first, first + 1, mid, last - 1)`.
    let (a, b, c) = (first + 1, mid, last - 1);
    let median = if less(&v[a], &v[b]) {
        if less(&v[b], &v[c]) {
            b
        } else if less(&v[a], &v[c]) {
            c
        } else {
            a
        }
    } else if less(&v[a], &v[c]) {
        a
    } else if less(&v[b], &v[c]) {
        c
    } else {
        b
    };
    v.swap(first, median);
    // `std::__unguarded_partition(first + 1, last, first)`.
    let (mut lo, mut hi) = (first + 1, last);
    loop {
        while less(&v[lo], &v[first]) {
            lo += 1;
        }
        hi -= 1;
        while less(&v[first], &v[hi]) {
            hi -= 1;
        }
        if lo >= hi {
            return lo;
        }
        v.swap(lo, hi);
        lo += 1;
    }
}

fn cpp_final_insertion_sort<E: Copy>(
    v: &mut [E],
    first: usize,
    last: usize,
    less: &impl Fn(&E, &E) -> bool,
) {
    if last - first > CPP_SORT_THRESHOLD {
        cpp_insertion_sort(v, first, first + CPP_SORT_THRESHOLD, less);
        // `std::__unguarded_insertion_sort`: the first sixteen hold the minimum, so no bound.
        for i in first + CPP_SORT_THRESHOLD..last {
            cpp_unguarded_linear_insert(v, i, less);
        }
    } else {
        cpp_insertion_sort(v, first, last, less);
    }
}

fn cpp_insertion_sort<E: Copy>(
    v: &mut [E],
    first: usize,
    last: usize,
    less: &impl Fn(&E, &E) -> bool,
) {
    if first == last {
        return;
    }
    for i in first + 1..last {
        if less(&v[i], &v[first]) {
            let value = v[i];
            v.copy_within(first..i, first + 1);
            v[first] = value;
        } else {
            cpp_unguarded_linear_insert(v, i, less);
        }
    }
}

fn cpp_unguarded_linear_insert<E: Copy>(
    v: &mut [E],
    mut last: usize,
    less: &impl Fn(&E, &E) -> bool,
) {
    let value = v[last];
    while less(&value, &v[last - 1]) {
        v[last] = v[last - 1];
        last -= 1;
    }
    v[last] = value;
}

// `std::__make_heap`.
fn cpp_make_heap<E: Copy>(v: &mut [E], less: &impl Fn(&E, &E) -> bool) {
    let len = v.len() as isize;
    if len < 2 {
        return;
    }
    let mut parent = (len - 2) / 2;
    loop {
        let value = v[parent as usize];
        cpp_adjust_heap(v, parent, len, value, less);
        if parent == 0 {
            return;
        }
        parent -= 1;
    }
}

// `std::__sort_heap`.
fn cpp_sort_heap<E: Copy>(v: &mut [E], less: &impl Fn(&E, &E) -> bool) {
    let mut last = v.len() as isize;
    while last > 1 {
        last -= 1;
        // `std::__pop_heap(first, last, last)`.
        let value = v[last as usize];
        v[last as usize] = v[0];
        cpp_adjust_heap(v, 0, last, value, less);
    }
}

// `std::__adjust_heap`, and the `std::__push_heap` it ends in. Signed, as the library's
// difference type is: `(hole - 1) / 2` at the root is zero there, not a wrapped index.
fn cpp_adjust_heap<E: Copy>(
    v: &mut [E],
    mut hole: isize,
    len: isize,
    value: E,
    less: &impl Fn(&E, &E) -> bool,
) {
    let top = hole;
    let mut second = hole;
    while second < (len - 1) / 2 {
        second = 2 * (second + 1);
        if less(&v[second as usize], &v[(second - 1) as usize]) {
            second -= 1;
        }
        v[hole as usize] = v[second as usize];
        hole = second;
    }
    if len & 1 == 0 && second == (len - 2) / 2 {
        second = 2 * (second + 1);
        v[hole as usize] = v[(second - 1) as usize];
        hole = second - 1;
    }
    let mut parent = (hole - 1) / 2;
    while hole > top && less(&v[parent as usize], &value) {
        v[hole as usize] = v[parent as usize];
        hole = parent;
        parent = (hole - 1) / 2;
    }
    v[hole as usize] = value;
}

// find the leftmost node of a polygon ring
//
// tessella: `earcut.hpp`'s `getLeftmost`: least x, and least y among those, walking from the node
// `linkedList` returned. Upstream took the first point of least x in insertion order, which picks
// a different bridge wherever a hole has a vertical left edge. See PATCH.md.
fn get_leftmost<T: Float>(ll: &LinkedLists<T>, start: LinkedListNodeIndex) -> LinkedListNodeIndex {
    let mut p = start;
    let mut leftmost = start;
    loop {
        let (pc, lc) = (ll.nodes[p].coord, ll.nodes[leftmost].coord);
        if pc.x < lc.x || (pc.x == lc.x && pc.y < lc.y) {
            leftmost = p;
        }
        p = ll.nodes[p].next_linked_list_node_index;
        if p == start {
            return leftmost;
        }
    }
}

// main ear slicing loop which triangulates a polygon (given as a linked
// list)
fn earcut_linked_hashed<const PASS: usize, T: Float>(
    ll: &mut LinkedLists<T>,
    mut ear_idx: LinkedListNodeIndex,
    triangle_indices: &mut FinalTriangleIndices,
) -> Result<(), Error> {
    // interlink polygon nodes in z-order
    if PASS == 0 {
        ll.index_curve(ear_idx)?;
    }
    // iterate through ears, slicing them one by one
    let mut stop_idx = ear_idx;
    let mut prev_idx = 0;
    let mut next_idx = ll.nodes[ear_idx].next_linked_list_node_index;
    while stop_idx != next_idx {
        prev_idx = ll.nodes[ear_idx].prev_linked_list_node_index;
        next_idx = ll.nodes[ear_idx].next_linked_list_node_index;
        let node_index_triangle = NodeIndexTriangle(prev_idx, ear_idx, next_idx);
        if node_index_triangle.node_triangle(ll).is_ear_hashed(ll)? {
            triangle_indices.push(VerticesIndexTriangle(
                ll.nodes[prev_idx].vertices_index,
                ll.nodes[ear_idx].vertices_index,
                ll.nodes[next_idx].vertices_index,
            ));
            ll.remove_node(ear_idx);
            // skipping the next vertex leads to less sliver triangles
            ear_idx = ll.nodes[next_idx].next_linked_list_node_index;
            stop_idx = ear_idx;
        } else {
            ear_idx = next_idx;
        }
    }

    if prev_idx == next_idx {
        return Ok(());
    };
    // if we looped through the whole remaining polygon and can't
    // find any more ears
    if PASS == 0 {
        let tmp = filter_points(ll, next_idx, None);
        earcut_linked_hashed::<1, T>(ll, tmp, triangle_indices)?;
    } else if PASS == 1 {
        // tessella: filtered first, as `earcut.hpp`'s pass 1 is.
        let filtered = filter_points(ll, next_idx, None);
        ear_idx = cure_local_intersections(ll, filtered, triangle_indices);
        earcut_linked_hashed::<2, T>(ll, ear_idx, triangle_indices)?;
    } else if PASS == 2 {
        split_earcut(ll, next_idx, triangle_indices)?;
    }
    Ok(())
}

// main ear slicing loop which triangulates a polygon (given as a linked
// list)
fn earcut_linked_unhashed<const PASS: usize, T: Float>(
    ll: &mut LinkedLists<T>,
    mut ear_idx: LinkedListNodeIndex,
    triangles: &mut FinalTriangleIndices,
) -> Result<(), Error> {
    // iterate through ears, slicing them one by one
    let mut stop_idx = ear_idx;
    let mut prev_idx = 0;
    let mut next_idx = ll.nodes[ear_idx].next_linked_list_node_index;
    while stop_idx != next_idx {
        prev_idx = ll.nodes[ear_idx].prev_linked_list_node_index;
        next_idx = ll.nodes[ear_idx].next_linked_list_node_index;
        if NodeIndexTriangle(prev_idx, ear_idx, next_idx).is_ear(ll) {
            triangles.push(VerticesIndexTriangle(
                ll.nodes[prev_idx].vertices_index,
                ll.nodes[ear_idx].vertices_index,
                ll.nodes[next_idx].vertices_index,
            ));
            ll.remove_node(ear_idx);
            // skipping the next vertex leads to less sliver triangles
            ear_idx = ll.nodes[next_idx].next_linked_list_node_index;
            stop_idx = ear_idx;
        } else {
            ear_idx = next_idx;
        }
    }

    if prev_idx == next_idx {
        return Ok(());
    };
    // if we looped through the whole remaining polygon and can't
    // find any more ears
    if PASS == 0 {
        let tmp = filter_points(ll, next_idx, None);
        earcut_linked_unhashed::<1, T>(ll, tmp, triangles)?;
    } else if PASS == 1 {
        // tessella: filtered first, as `earcut.hpp`'s pass 1 is.
        let filtered = filter_points(ll, next_idx, None);
        ear_idx = cure_local_intersections(ll, filtered, triangles);
        earcut_linked_unhashed::<2, T>(ll, ear_idx, triangles)?;
    } else if PASS == 2 {
        split_earcut(ll, next_idx, triangles)?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct NodeIndexTriangle(
    LinkedListNodeIndex,
    LinkedListNodeIndex,
    LinkedListNodeIndex,
);

impl NodeIndexTriangle {
    fn prev_node<T: Float>(self, ll: &LinkedLists<T>) -> LinkedListNode<T> {
        ll.nodes[self.0]
    }

    fn ear_node<T: Float>(self, ll: &LinkedLists<T>) -> LinkedListNode<T> {
        ll.nodes[self.1]
    }

    fn next_node<T: Float>(self, ll: &LinkedLists<T>) -> LinkedListNode<T> {
        ll.nodes[self.2]
    }

    fn node_triangle<T: Float>(self, ll: &LinkedLists<T>) -> NodeTriangle<T> {
        NodeTriangle(self.prev_node(ll), self.ear_node(ll), self.next_node(ll))
    }

    // check whether a polygon node forms a valid ear with adjacent nodes
    //
    // tessella: `earcut.hpp`'s `isEar`. The triangle's corners are read once and the ring is walked
    // by index from the node after `next`, stopping *before* testing `prev` -- so on a ring of
    // three nothing is tested and the ear is taken. Upstream tested its first node
    // unconditionally, which on a ring of three is `prev`, whose own triangle is this one rotated:
    // its area rounds to the other sign often enough on non-integer coordinates to refuse the last
    // ear of a polygon. See PATCH.md.
    fn is_ear<T: Float>(self, ll: &LinkedLists<T>) -> bool {
        let zero = T::zero();
        let a = ll.nodes[self.0].coord;
        let b = ll.nodes[self.1].coord;
        let c = ll.nodes[self.2].coord;
        if coord_area(a, b, c) >= zero {
            return false; // reflex, cant be ear
        }
        let end = ll.nodes[self.0].idx;
        let mut p = ll.nodes[self.2].next_linked_list_node_index;
        while p != end {
            let node = &ll.nodes[p];
            if point_in_triangle(a, b, c, node.coord)
                && coord_area(
                    ll.nodes[node.prev_linked_list_node_index].coord,
                    node.coord,
                    ll.nodes[node.next_linked_list_node_index].coord,
                ) >= zero
            {
                return false;
            }
            p = node.next_linked_list_node_index;
        }
        true
    }
}

#[derive(Clone, Copy)]
struct NodeTriangle<T: Float>(LinkedListNode<T>, LinkedListNode<T>, LinkedListNode<T>);

impl<T: Float> NodeTriangle<T> {
    fn from_ear_node(ear_node: LinkedListNode<T>, ll: &LinkedLists<T>) -> Self {
        NodeTriangle(
            ear_node.prev_linked_list_node(ll),
            ear_node,
            ear_node.next_linked_list_node(ll),
        )
    }

    fn area(&self) -> T {
        coord_area(self.0.coord, self.1.coord, self.2.coord)
    }

    // check if a point lies within a convex triangle
    fn contains_point(&self, p: LinkedListNode<T>) -> bool {
        point_in_triangle(self.0.coord, self.1.coord, self.2.coord, p.coord)
    }

    #[inline(always)]
    fn is_ear_hashed(&self, ll: &mut LinkedLists<T>) -> Result<bool, Error> {
        let zero = T::zero();

        if self.area() >= zero {
            return Ok(false);
        };
        let NodeTriangle(prev, ear, next) = self;

        let bbox_maxx = prev.coord.x.max(ear.coord.x.max(next.coord.x));
        let bbox_maxy = prev.coord.y.max(ear.coord.y.max(next.coord.y));
        let bbox_minx = prev.coord.x.min(ear.coord.x.min(next.coord.x));
        let bbox_miny = prev.coord.y.min(ear.coord.y.min(next.coord.y));
        // z-order range for the current triangle bbox;
        let min_z = ll.zorder(bbox_minx, bbox_miny);
        let max_z = ll.zorder(bbox_maxx, bbox_maxy);

        let mut p = ear.prevz_idx;
        let mut n = ear.nextz_idx;
        while (p != NULL) && (ll.nodes[p].z >= min_z) && (n != NULL) && (ll.nodes[n].z <= max_z) {
            if earcheck(
                prev,
                ear,
                next,
                prevref!(ll, p),
                &ll.nodes[p],
                nextref!(ll, p),
            ) {
                return Ok(false);
            }
            p = ll.nodes[p].prevz_idx;

            if earcheck(
                prev,
                ear,
                next,
                prevref!(ll, n),
                &ll.nodes[n],
                nextref!(ll, n),
            ) {
                return Ok(false);
            }
            n = ll.nodes[n].nextz_idx;
        }

        ll.nodes[NULL].z = min_z - 1;
        while ll.nodes[p].z >= min_z {
            if earcheck(
                prev,
                ear,
                next,
                prevref!(ll, p),
                &ll.nodes[p],
                nextref!(ll, p),
            ) {
                return Ok(false);
            }
            p = ll.nodes[p].prevz_idx;
        }

        ll.nodes[NULL].z = max_z + 1;
        while ll.nodes[n].z <= max_z {
            if earcheck(
                prev,
                ear,
                next,
                prevref!(ll, n),
                &ll.nodes[n],
                nextref!(ll, n),
            ) {
                return Ok(false);
            }
            n = ll.nodes[n].nextz_idx;
        }

        Ok(true)
    }
}

// signed area of a parallelogram
//
// tessella: `NodeTriangle::area`'s expression, on coordinates, so a caller holding coordinates need
// not copy whole nodes to ask. The same operations in the same order: Rust does not contract a
// multiply and add into one rounding unless asked, so the result is the same bits.
#[inline(always)]
fn coord_area<T: Float>(p: Coord<T>, q: Coord<T>, r: Coord<T>) -> T {
    (q.y - p.y) * (r.x - q.x) - (q.x - p.x) * (r.y - q.y)
}

// check if a point lies within a convex triangle
//
// tessella: `NodeTriangle::contains_point`'s expression, on coordinates, as `coord_area` is.
#[inline(always)]
fn point_in_triangle<T: Float>(a: Coord<T>, b: Coord<T>, c: Coord<T>, p: Coord<T>) -> bool {
    let zero = T::zero();
    ((c.x - p.x) * (a.y - p.y) - (a.x - p.x) * (c.y - p.y) >= zero)
        && ((a.x - p.x) * (b.y - p.y) - (b.x - p.x) * (a.y - p.y) >= zero)
        && ((b.x - p.x) * (c.y - p.y) - (c.x - p.x) * (b.y - p.y) >= zero)
}

// helper for is_ear_hashed. needs manual inline (rust 2018)
#[inline(always)]
fn earcheck<T: Float>(
    a: &LinkedListNode<T>,
    b: &LinkedListNode<T>,
    c: &LinkedListNode<T>,
    prev: &LinkedListNode<T>,
    p: &LinkedListNode<T>,
    next: &LinkedListNode<T>,
) -> bool {
    let zero = T::zero();

    (p.idx != a.idx)
        && (p.idx != c.idx)
        && NodeTriangle(*a, *b, *c).contains_point(*p)
        && NodeTriangle(*prev, *p, *next).area() >= zero
}

fn filter_points<T: Float>(
    ll: &mut LinkedLists<T>,
    start: LinkedListNodeIndex,
    end: Option<LinkedListNodeIndex>,
) -> LinkedListNodeIndex {
    let mut end = end.unwrap_or(start);
    if end >= ll.nodes.len() || start >= ll.nodes.len() {
        return NULL;
    }

    let mut p = start;
    let mut again;

    // this loop "wastes" calculations by going over the same points multiple
    // times. however, altering the location of the 'end' node can disrupt
    // the algorithm of other code that calls the filter_points function.
    loop {
        again = false;
        if !ll.nodes[p].is_steiner_point
            && (ll.nodes[p].xy_eq(ll.nodes[ll.nodes[p].next_linked_list_node_index])
                || NodeTriangle::from_ear_node(ll.nodes[p], ll)
                    .area()
                    .is_zero())
        {
            ll.remove_node(p);
            end = ll.nodes[p].prev_linked_list_node_index;
            p = end;
            if p == ll.nodes[p].next_linked_list_node_index {
                break end;
            }
            again = true;
        } else {
            // tessella: no early NULL for a ring of one steiner point; `earcut.hpp` walks on and
            // returns `end`, which is where the walk below stops.
            p = ll.nodes[p].next_linked_list_node_index;
        }
        if !again && p == end {
            break end;
        }
    }
}

// create a circular doubly linked list from polygon points in the
// specified winding order
//
// tessella: `nodes` is how many the list will hold, so it is allocated once. Upstream reserved one
// per point and then pushed the NULL node as well, which reallocated and copied every list at least
// once, and a hole's bridge adds two more.
fn linked_list<T: Float>(
    vertices: &Vertices<T>,
    start: usize,
    end: usize,
    clockwise: bool,
    nodes: usize,
) -> Result<(LinkedLists<T>, LinkedListNodeIndex), Error> {
    let mut ll: LinkedLists<T> = LinkedLists::new(nodes);
    // Point count, not the length of the flat coordinate array. earcut.hpp counts points --
    // `threshold = 80` decremented by each ring's size, `hashing = threshold < 0` -- so it hashes
    // above eighty *points*. Comparing `vertices.len()` against the same eighty compares twice
    // that number and hashes above forty, which puts every polygon of 41..80 points on the other
    // implementation's branch. See PATCH.md.
    if vertices.len() / DIM <= 80 {
        ll.usehash = false;
    };
    let last_idx = ll.add_contour(vertices, start, end, clockwise)?;
    Ok((ll, last_idx))
}

struct VerticesIndexTriangle(usize, usize, usize);

#[derive(Default, Debug)]
struct FinalTriangleIndices(Vec<usize>);

impl FinalTriangleIndices {
    fn push(&mut self, vertices_index_triangle: VerticesIndexTriangle) {
        self.0.push(vertices_index_triangle.0);
        self.0.push(vertices_index_triangle.1);
        self.0.push(vertices_index_triangle.2);
    }
}

pub fn earcut<T: Float>(
    vertices: &[T],
    hole_indices: &[VerticesIndex],
    dims: usize,
) -> Result<Vec<usize>, Error> {
    if vertices.is_empty() && hole_indices.is_empty() {
        return Ok(vec![]);
    }

    if vertices.len() % 2 == 1 || dims > vertices.len() {
        return Err(Error::Unknown);
    }

    let outer_len = match hole_indices.first() {
        Some(first_hole_index) => {
            let outer_len = first_hole_index.checked_mul(DIM).ok_or(Error::Unknown)?;
            if outer_len > vertices.len() || outer_len == 0 {
                return Err(Error::Unknown);
            }
            if outer_len % 2 == 1 {
                return Err(Error::Unknown);
            }
            outer_len
        }
        None => vertices.len(),
    };

    let vertices = Vertices(vertices);
    // tessella: sized for what they will hold -- the NULL node, one node per point and two per
    // hole's bridge; and three indices per triangle, of which a polygon has about one per node.
    // Upstream reserved a third of the triangles, and grew the rest.
    let nodes = 1 + vertices.len() / DIM + 2 * hole_indices.len();
    let (mut ll, outer_node) = linked_list(&vertices, 0, outer_len, true, nodes)?;
    let mut triangles = FinalTriangleIndices(Vec::with_capacity(3 * nodes));
    if ll.nodes.len() == 1 || DIM != dims {
        return Ok(triangles.0);
    }
    // tessella: an outer ring of one or two points has nothing to triangulate, and `earcut.hpp`
    // stops here, before its holes are bridged in. Upstream bridged them and triangulated those.
    if ll.nodes[outer_node].prev_linked_list_node_index
        == ll.nodes[outer_node].next_linked_list_node_index
    {
        return Ok(triangles.0);
    }

    let outer_node = eliminate_holes(&mut ll, &vertices, hole_indices, outer_node)?;

    if ll.usehash {
        // tessella: `earcut.hpp`'s box -- the outer ring's, walked once the holes are bridged into
        // it -- and its `inv_size`, the reciprocal of the longer side. The coordinates are left
        // where they are: `zorder` subtracts the minimum itself, as the C++ does, so every area and
        // containment test sees the points the caller gave. See PATCH.md.
        let first = ll.nodes[outer_node].coord;
        let (mut min, mut max) = (first, first);
        let mut p = ll.nodes[outer_node].next_linked_list_node_index;
        loop {
            let c = ll.nodes[p].coord;
            min.x = cpp_min(min.x, c.x);
            min.y = cpp_min(min.y, c.y);
            max.x = cpp_max(max.x, c.x);
            max.y = cpp_max(max.y, c.y);
            p = ll.nodes[p].next_linked_list_node_index;
            if p == outer_node {
                break;
            }
        }
        let size = cpp_max(max.x - min.x, max.y - min.y);
        ll.min = min;
        ll.max = max;
        ll.invsize = if size != T::zero() {
            T::one() / size
        } else {
            T::zero()
        };
        earcut_linked_hashed::<0, T>(&mut ll, outer_node, &mut triangles)?;
    } else {
        earcut_linked_unhashed::<0, T>(&mut ll, outer_node, &mut triangles)?;
    }

    Ok(triangles.0)
}

/* go through all polygon nodes and cure small local self-intersections
what is a small local self-intersection? well, lets say you have four points
a,b,c,d. now imagine you have three line segments, a-b, b-c, and c-d. now
imagine two of those segments overlap each other. thats an intersection. so
this will remove one of those nodes so there is no more overlap.

but theres another important aspect of this function. it will dump triangles
into the 'triangles' variable, thus this is part of the triangulation
algorithm itself.*/
fn cure_local_intersections<T: Float>(
    ll: &mut LinkedLists<T>,
    instart: LinkedListNodeIndex,
    triangles: &mut FinalTriangleIndices,
) -> LinkedListNodeIndex {
    let mut p = instart;
    let mut start = instart;

    //        2--3  4--5 << 2-3 + 4-5 pseudointersects
    //           x  x
    //  0  1  2  3  4  5  6  7
    //  a  p  pn b
    //              eq     a      b
    //              psi    a p pn b
    //              li  pa a p pn b bn
    //              tp     a p    b
    //              rn       p pn
    //              nst    a      p pn b
    //                            st

    //
    //                            a p  pn b

    loop {
        let a = ll.nodes[p].prev_linked_list_node_index;
        let b = next!(ll, p).next_linked_list_node_index;

        // tessella: `earcut.hpp`'s `intersects`, which counts touching and collinear-overlapping
        // segments; upstream's `pseudo_intersects` counted only proper crossings. See PATCH.md.
        if !ll.nodes[a].xy_eq(ll.nodes[b])
            && intersects(
                ll.nodes[a].coord,
                ll.nodes[p].coord,
                nextref!(ll, p).coord,
                ll.nodes[b].coord,
            )
            && locally_inside(ll, &ll.nodes[a], &ll.nodes[b])
            && locally_inside(ll, &ll.nodes[b], &ll.nodes[a])
        {
            triangles.push(VerticesIndexTriangle(
                ll.nodes[a].vertices_index,
                ll.nodes[p].vertices_index,
                ll.nodes[b].vertices_index,
            ));

            // remove two nodes involved
            ll.remove_node(p);
            let nidx = ll.nodes[p].next_linked_list_node_index;
            ll.remove_node(nidx);

            start = ll.nodes[b].idx;
            p = start;
        }
        p = ll.nodes[p].next_linked_list_node_index;
        if p == start {
            break;
        }
    }

    // tessella: filtered on the way out, as `earcut.hpp` returns `filterPoints(p)`.
    filter_points(ll, p, None)
}

// try splitting polygon into two and triangulate them independently
fn split_earcut<T: Float>(
    ll: &mut LinkedLists<T>,
    start_idx: LinkedListNodeIndex,
    triangles: &mut FinalTriangleIndices,
) -> Result<(), Error> {
    // look for a valid diagonal that divides the polygon into two
    let mut a = start_idx;
    loop {
        let mut b = next!(ll, a).next_linked_list_node_index;
        while b != ll.nodes[a].prev_linked_list_node_index {
            if ll.nodes[a].vertices_index != ll.nodes[b].vertices_index
                && ll.is_valid_diagonal(&ll.nodes[a], &ll.nodes[b])
            {
                // split the polygon in two by the diagonal
                let mut c = split_bridge_polygon(ll, a, b);

                // filter colinear points around the cuts
                let an = ll.nodes[a].next_linked_list_node_index;
                let cn = ll.nodes[c].next_linked_list_node_index;
                a = filter_points(ll, a, Some(an));
                c = filter_points(ll, c, Some(cn));

                // run earcut on each half
                //
                // tessella: in the mode the polygon is in. Upstream always ran the hashed loop,
                // which for an unhashed polygon searched a z-order that was never built.
                if ll.usehash {
                    earcut_linked_hashed::<0, T>(ll, a, triangles)?;
                    earcut_linked_hashed::<0, T>(ll, c, triangles)?;
                } else {
                    earcut_linked_unhashed::<0, T>(ll, a, triangles)?;
                    earcut_linked_unhashed::<0, T>(ll, c, triangles)?;
                }
                return Ok(());
            }
            b = ll.nodes[b].next_linked_list_node_index;
        }
        a = ll.nodes[a].next_linked_list_node_index;
        if a == start_idx {
            break;
        }
    }
    Ok(())
}

// David Eberly's algorithm for finding a bridge between hole and outer polygon
fn find_hole_bridge<T: Float>(
    ll: &LinkedLists<T>,
    hole: LinkedListNodeIndex,
    outer_node: LinkedListNodeIndex,
) -> LinkedListNodeIndex {
    // tessella: `earcut.hpp`'s `findHoleBridge`, statement for statement. Upstream followed an
    // older `earcut.js`: it answered `m.prev` where a hole touches an outer segment, began its
    // second pass after `m` from a finite minimum, and broke ties on x alone. Each of those can
    // pick a different bridge, and a different bridge is different triangles. See PATCH.md.
    let mut p = outer_node;
    let hx = ll.nodes[hole].coord.x;
    let hy = ll.nodes[hole].coord.y;
    let mut qx = T::neg_infinity();
    let mut m = NULL;

    // find a segment intersected by a ray from the hole's leftmost Vertex to the left;
    // segment's endpoint with lesser x will be potential connection Vertex
    loop {
        let (pc, next) = (ll.nodes[p].coord, ll.nodes[p].next_linked_list_node_index);
        let nc = ll.nodes[next].coord;
        if hy <= pc.y && hy >= nc.y && nc.y != pc.y {
            let x = pc.x + (hy - pc.y) * (nc.x - pc.x) / (nc.y - pc.y);
            if x <= hx && x > qx {
                qx = x;
                if x == hx {
                    if hy == pc.y {
                        return p;
                    }
                    if hy == nc.y {
                        return next;
                    }
                }
                m = if pc.x < nc.x { p } else { next };
            }
        }
        p = next;
        if p == outer_node {
            break;
        }
    }

    if m == NULL {
        return NULL;
    }

    if hx == qx {
        return m; // hole touches outer segment; pick leftmost endpoint
    }

    // look for points inside the triangle of hole Vertex, segment intersection and endpoint;
    // if there are no points found, we have a valid connection;
    // otherwise choose the Vertex of the minimum angle with the ray as connection Vertex
    let stop = m;
    let mut tan_min = T::infinity();
    let mc = ll.nodes[m].coord;
    let (mx, my) = (mc.x, mc.y);
    let a = Coord {
        x: if hy < my { hx } else { qx },
        y: hy,
    };
    let c = Coord {
        x: if hy < my { qx } else { hx },
        y: hy,
    };

    p = m;
    loop {
        let pc = ll.nodes[p].coord;
        if hx >= pc.x && pc.x >= mx && hx != pc.x && point_in_triangle(a, mc, c, pc) {
            let tan_cur = (hy - pc.y).abs() / (hx - pc.x); // tangential

            if locally_inside(ll, &ll.nodes[p], &ll.nodes[hole])
                && (tan_cur < tan_min
                    || (tan_cur == tan_min
                        && (pc.x > ll.nodes[m].coord.x || sector_contains_sector(ll, m, p))))
            {
                m = p;
                tan_min = tan_cur;
            }
        }

        p = ll.nodes[p].next_linked_list_node_index;
        if p == stop {
            break;
        }
    }

    m
}

/* check if two segments cross over each other. note this is different
from pure intersction. only two segments crossing over at some interior
point is considered intersection.

line segment p1-q1 vs line segment p2-q2.

note that if they are collinear, or if the end points touch, or if
one touches the other at one point, it is not considered an intersection.

please note that the other algorithms in this earcut code depend on this
interpretation of the concept of intersection - if this is modified
so that endpoint touching qualifies as intersection, then it will have
a problem with certain inputs.

bsed on https://www.geeksforgeeks.org/check-if-two-given-line-segments-intersect/

this has been modified from the version in earcut.js to remove the
detection for endpoint detection.

    a1=area(p1,q1,p2);a2=area(p1,q1,q2);a3=area(p2,q2,p1);a4=area(p2,q2,q1);
    p1 q1    a1 cw   a2 cw   a3 ccw   a4  ccw  a1==a2  a3==a4  fl
    p2 q2
    p1 p2    a1 ccw  a2 ccw  a3 cw    a4  cw   a1==a2  a3==a4  fl
    q1 q2
    p1 q2    a1 ccw  a2 ccw  a3 ccw   a4  ccw  a1==a2  a3==a4  fl
    q1 p2
    p1 q2    a1 cw   a2 ccw  a3 ccw   a4  cw   a1!=a2  a3!=a4  tr
    p2 q1
*/

// check if two segments intersect
//
// tessella: `earcut.hpp`'s `intersects`: the general case by orientation signs, and the four
// collinear cases by `on_segment`, so segments that touch or overlap count as intersecting.
// Upstream's `pseudo_intersects` counted proper crossings, and coincident segments. See PATCH.md.
fn intersects<T: Float>(p1: Coord<T>, q1: Coord<T>, p2: Coord<T>, q2: Coord<T>) -> bool {
    let o1 = sign(coord_area(p1, q1, p2));
    let o2 = sign(coord_area(p1, q1, q2));
    let o3 = sign(coord_area(p2, q2, p1));
    let o4 = sign(coord_area(p2, q2, q1));

    if o1 != o2 && o3 != o4 {
        return true; // general case
    }

    (o1 == 0 && on_segment(p1, p2, q1)) // p1, q1 and p2 are collinear and p2 lies on p1q1
        || (o2 == 0 && on_segment(p1, q2, q1)) // p1, q1 and q2 are collinear and q2 lies on p1q1
        || (o3 == 0 && on_segment(p2, p1, q2)) // p2, q2 and p1 are collinear and p1 lies on p2q2
        || (o4 == 0 && on_segment(p2, q1, q2)) // p2, q2 and q1 are collinear and q1 lies on p2q2
}

// for collinear points p, q, r, check if point q lies on segment pr
fn on_segment<T: Float>(p: Coord<T>, q: Coord<T>, r: Coord<T>) -> bool {
    q.x <= cpp_max(p.x, r.x)
        && q.x >= cpp_min(p.x, r.x)
        && q.y <= cpp_max(p.y, r.y)
        && q.y >= cpp_min(p.y, r.y)
}

// `earcut.hpp`'s `sign`: -1, 0 or 1.
fn sign<T: Float>(value: T) -> i32 {
    i32::from(T::zero() < value) - i32::from(value < T::zero())
}

// `std::min` and `std::max` as `earcut.hpp` calls them, which on a tie answer the first argument.
fn cpp_min<T: Float>(a: T, b: T) -> T {
    if b < a {
        b
    } else {
        a
    }
}

fn cpp_max<T: Float>(a: T, b: T) -> T {
    if a < b {
        b
    } else {
        a
    }
}

// whether sector in vertex m contains sector in vertex p in the same coordinates
//
// tessella: `earcut.hpp`'s `sectorContainsSector`, which `find_hole_bridge` breaks ties with.
fn sector_contains_sector<T: Float>(
    ll: &LinkedLists<T>,
    m: LinkedListNodeIndex,
    p: LinkedListNodeIndex,
) -> bool {
    let zero = T::zero();
    let (mp, mc, mn) = (prev!(ll, m).coord, ll.nodes[m].coord, next!(ll, m).coord);
    let (pp, pn) = (prev!(ll, p).coord, next!(ll, p).coord);
    (coord_area(mp, mc, pp) < zero || coord_area(pp, mc, mn) < zero)
        && (coord_area(mp, mc, pn) < zero || coord_area(pn, mc, mn) < zero)
}

// check if a polygon diagonal intersects any polygon segments
fn intersects_polygon<T: Float>(
    ll: &LinkedLists<T>,
    a: LinkedListNode<T>,
    b: LinkedListNode<T>,
) -> bool {
    ll.iter_pairs(a.idx..a.idx).any(|(p, n)| {
        p.vertices_index != a.vertices_index
            && n.vertices_index != a.vertices_index
            && p.vertices_index != b.vertices_index
            && n.vertices_index != b.vertices_index
            && intersects(p.coord, n.coord, a.coord, b.coord)
    })
}

// check if a polygon diagonal is locally inside the polygon
fn locally_inside<T: Float>(
    ll: &LinkedLists<T>,
    a: &LinkedListNode<T>,
    b: &LinkedListNode<T>,
) -> bool {
    let zero = T::zero();

    match NodeTriangle(*prevref!(ll, a.idx), *a, *nextref!(ll, a.idx)).area() < zero {
        true => {
            NodeTriangle(*a, *b, *nextref!(ll, a.idx)).area() >= zero
                && NodeTriangle(*a, *prevref!(ll, a.idx), *b).area() >= zero
        }
        false => {
            NodeTriangle(*a, *b, *prevref!(ll, a.idx)).area() < zero
                || NodeTriangle(*a, *nextref!(ll, a.idx), *b).area() < zero
        }
    }
}

// check if the middle point of a polygon diagonal is inside the polygon
fn middle_inside<T: Float>(
    ll: &LinkedLists<T>,
    a: &LinkedListNode<T>,
    b: &LinkedListNode<T>,
) -> bool {
    let two = T::one() + T::one();

    let (mx, my) = ((a.coord.x + b.coord.x) / two, (a.coord.y + b.coord.y) / two);
    ll.iter_pairs(a.idx..a.idx)
        .filter(|(p, n)| (p.coord.y > my) != (n.coord.y > my))
        .filter(|(p, n)| n.coord.y != p.coord.y)
        .filter(|(p, n)| {
            (mx) < ((n.coord.x - p.coord.x) * (my - p.coord.y) / (n.coord.y - p.coord.y)
                + p.coord.x)
        })
        .fold(false, |inside, _| !inside)
}

/* link two polygon vertices with a bridge;

if the vertices belong to the same linked list, this splits the list
into two new lists, representing two new polygons.

if the vertices belong to separate linked lists, it merges them into a
single linked list.

For example imagine 6 points, labeled with numbers 0 thru 5, in a single cycle.
Now split at points 1 and 4. The 2 new polygon cycles will be like this:
0 1 4 5 0 1 ...  and  1 2 3 4 1 2 3 .... However because we are using linked
lists of nodes, there will be two new nodes, copies of points 1 and 4. So:
the new cycles will be through nodes 0 1 4 5 0 1 ... and 2 3 6 7 2 3 6 7 .

splitting algorithm:

.0...1...2...3...4...5...     6     7
5p1 0a2 1m3 2n4 3b5 4q0      .c.   .d.

an<-2     an = a.next,
bp<-3     bp = b.prev;
1.n<-4    a.next = b;
4.p<-1    b.prev = a;
6.n<-2    c.next = an;
2.p<-6    an.prev = c;
7.n<-6    d.next = c;
6.p<-7    c.prev = d;
3.n<-7    bp.next = d;
7.p<-3    d.prev = bp;

result of split:
<0...1> <2...3> <4...5>      <6....7>
5p1 0a4 6m3 2n7 1b5 4q0      7c2  3d6
      x x     x x            x x  x x    // x shows links changed

a b q p a b q p  // begin at a, go next (new cycle 1)
a p q b a p q b  // begin at a, go prev (new cycle 1)
m n d c m n d c  // begin at m, go next (new cycle 2)
m c d n m c d n  // begin at m, go prev (new cycle 2)

Now imagine that we have two cycles, and
they are 0 1 2, and 3 4 5. Split at points 1 and
4 will result in a single, long cycle,
0 1 4 5 3 7 6 2 0 1 4 5 ..., where 6 and 1 have the
same x y f64s, as do 7 and 4.

 0...1...2   3...4...5        6     7
2p1 0a2 1m0 5n4 3b5 4q3      .c.   .d.

an<-2     an = a.next,
bp<-3     bp = b.prev;
1.n<-4    a.next = b;
4.p<-1    b.prev = a;
6.n<-2    c.next = an;
2.p<-6    an.prev = c;
7.n<-6    d.next = c;
6.p<-7    c.prev = d;
3.n<-7    bp.next = d;
7.p<-3    d.prev = bp;

result of split:
 0...1...2   3...4...5        6.....7
2p1 0a4 6m0 5n7 1b5 4q3      7c2   3d6
      x x     x x            x x   x x

a b q n d c m p a b q n d c m .. // begin at a, go next
a p m c d n q b a p m c d n q .. // begin at a, go prev

Return value.

Return value is the new node, at point 7.
*/
fn split_bridge_polygon<T: Float>(
    ll: &mut LinkedLists<T>,
    a: LinkedListNodeIndex,
    b: LinkedListNodeIndex,
) -> LinkedListNodeIndex {
    let cidx = ll.nodes.len();
    let didx = cidx + 1;
    let mut c = LinkedListNode::new(ll.nodes[a].vertices_index, ll.nodes[a].coord, cidx);
    let mut d = LinkedListNode::new(ll.nodes[b].vertices_index, ll.nodes[b].coord, didx);

    let an = ll.nodes[a].next_linked_list_node_index;
    let bp = ll.nodes[b].prev_linked_list_node_index;

    ll.nodes[a].next_linked_list_node_index = b;
    ll.nodes[b].prev_linked_list_node_index = a;

    c.next_linked_list_node_index = an;
    ll.nodes[an].prev_linked_list_node_index = cidx;

    d.next_linked_list_node_index = cidx;
    c.prev_linked_list_node_index = didx;

    ll.nodes[bp].next_linked_list_node_index = didx;
    d.prev_linked_list_node_index = bp;

    ll.nodes.push(c);
    ll.nodes.push(d);
    didx
}
