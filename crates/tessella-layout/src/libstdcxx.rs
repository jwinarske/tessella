//! `std::nth_element` as libstdc++ implements it.
//!
//! mbgl caps a polygon's holes with `std::nth_element`, which promises only that the nth element
//! lands where a sort would put it and that nothing after it compares ahead of it. Which elements
//! end up on either side when several compare equal, and in what order the kept ones sit, is the
//! library's business, and the oracle is built against libstdc++. The kept holes' order is not
//! incidental: it is the order their vertices are numbered in, and it breaks ties in the order
//! earcut bridges them. So this is libstdc++'s introselect, step for step -- the same median-of-
//! three partition its `std::sort` uses, a heap select past the depth limit, and an insertion sort
//! once a range is down to three.
//!
//! The vendored earcutr carries the same library's sort for the same reason; it cannot depend on
//! this crate, so each keeps the part it calls.

/// Rearranges `v` as libstdc++'s `std::nth_element(v.begin(), v.begin() + nth, v.end(), less)`.
pub(crate) fn nth_element<E: Copy>(v: &mut [E], nth: usize, less: &impl Fn(&E, &E) -> bool) {
    let len = v.len();
    if len == 0 || nth == len {
        return;
    }
    // `std::__lg(len) * 2`.
    let depth = 2 * (usize::BITS - 1 - len.leading_zeros()) as usize;
    introselect(v, 0, nth, len, depth, less);
}

fn introselect<E: Copy>(
    v: &mut [E],
    mut first: usize,
    nth: usize,
    mut last: usize,
    mut depth: usize,
    less: &impl Fn(&E, &E) -> bool,
) {
    while last - first > 3 {
        if depth == 0 {
            // `std::__heap_select(first, nth + 1, last)`, then the nth into place.
            heap_select(&mut v[first..last], nth + 1 - first, less);
            v.swap(first, nth);
            return;
        }
        depth -= 1;
        let cut = unguarded_partition_pivot(v, first, last, less);
        if cut <= nth {
            first = cut;
        } else {
            last = cut;
        }
    }
    insertion_sort(v, first, last, less);
}

/// `std::__unguarded_partition_pivot`: the median of three moved to `first`, then partitioned
/// around it.
fn unguarded_partition_pivot<E: Copy>(
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

/// `std::__insertion_sort`.
fn insertion_sort<E: Copy>(v: &mut [E], first: usize, last: usize, less: &impl Fn(&E, &E) -> bool) {
    if first == last {
        return;
    }
    for i in first + 1..last {
        let value = v[i];
        if less(&value, &v[first]) {
            v.copy_within(first..i, first + 1);
            v[first] = value;
        } else {
            // `std::__unguarded_linear_insert`: `v[first]` bounds the walk.
            let mut hole = i;
            while less(&value, &v[hole - 1]) {
                v[hole] = v[hole - 1];
                hole -= 1;
            }
            v[hole] = value;
        }
    }
}

/// `std::__heap_select(first, middle, last)`: a heap of the first `middle`, and anything after it
/// that compares ahead of the heap's top swapped in.
fn heap_select<E: Copy>(v: &mut [E], middle: usize, less: &impl Fn(&E, &E) -> bool) {
    make_heap(&mut v[..middle], less);
    for i in middle..v.len() {
        if less(&v[i], &v[0]) {
            // `std::__pop_heap(first, middle, i)`.
            let value = v[i];
            v[i] = v[0];
            adjust_heap(&mut v[..middle], 0, middle as isize, value, less);
        }
    }
}

/// `std::__make_heap`.
fn make_heap<E: Copy>(v: &mut [E], less: &impl Fn(&E, &E) -> bool) {
    let len = v.len() as isize;
    if len < 2 {
        return;
    }
    let mut parent = (len - 2) / 2;
    loop {
        let value = v[parent as usize];
        adjust_heap(v, parent, len, value, less);
        if parent == 0 {
            return;
        }
        parent -= 1;
    }
}

/// `std::__adjust_heap`, and the `std::__push_heap` it ends in. Signed, as the library's
/// difference type is: `(hole - 1) / 2` at the root is zero there, not a wrapped index.
fn adjust_heap<E: Copy>(
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
