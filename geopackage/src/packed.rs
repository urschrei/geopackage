//! Direct construction of SQLite RTree shadow-table contents.
//!
//! SQLite's RTree module has no bulk-load entry point, so the D8 build used to
//! insert every entry into a scratch RTree one row at a time and copy the
//! result. That insertion, not the surrounding work, dominated the build (3.03 s
//! of 4.87 s for 1M points), because each insert descends the tree and may split
//! nodes. This module skips the module entirely and writes the node blobs
//! itself, bottom-up, from a spatially sorted entry set.
//!
//! # Node format
//!
//! Taken from the RTree module's own documentation and its `rtreecheck`
//! implementation (SQLite 3.51.3, `sqlite3.c`). Every node blob is exactly
//! `node_size` bytes:
//!
//! ```text
//! bytes 0..2    big-endian u16: tree depth, root node only (0 = root is a leaf);
//!               unused on other nodes
//! bytes 2..4    big-endian u16: number of cells in this node
//! bytes 4..     cells, each 8 + 2*2*4 = 24 bytes for a 2-D index:
//!                 big-endian i64  rowid (leaf) or child node number (internal)
//!                 big-endian f32  min_x, max_x, min_y, max_y
//! remainder     zero padding
//! ```
//!
//! The root is always node 1. `%_rowid` maps each leaf entry's rowid to the node
//! holding it, and `%_parent` maps each non-root node to its parent.
//!
//! # What `rtreecheck` requires
//!
//! The gate runs `rtreecheck()` over the result, so the output must satisfy
//! exactly what it verifies: `4 + ncell * 24 <= node_size`; `min <= max` on
//! every dimension of every cell; every cell's bounds contained in its parent
//! cell's bounds; a `%_rowid` row per leaf cell and a `%_parent` row per
//! internal cell; and the counts of those two tables equal to the number of leaf
//! and internal cells respectively. It does **not** require any minimum node
//! fill, so nodes are packed full.

use crate::Result;

/// Bytes per cell in a 2-D RTree: an 8-byte id plus four 4-byte coordinates.
const BYTES_PER_CELL: usize = 24;
/// Bytes of node header: the depth/unused field and the cell count.
const NODE_HEADER: usize = 4;

/// Rounding constants from the RTree module, used to force a `f64` to a `f32`
/// that is no larger (`DOWN`) or no smaller (`UP`) than the original.
const RND_TOWARDS: f64 = 1.0 - 1.0 / 8_388_608.0;
const RND_AWAY: f64 = 1.0 + 1.0 / 8_388_608.0;

/// The largest `f32` that is `<= d`, matching the RTree module's
/// `rtreeValueDown`. Minimum bounds are rounded this way so the stored box never
/// excludes a value it should contain.
fn coord_down(d: f64) -> f32 {
    let f = d as f32;
    if f64::from(f) > d {
        (d * if d < 0.0 { RND_AWAY } else { RND_TOWARDS }) as f32
    } else {
        f
    }
}

/// The smallest `f32` that is `>= d`, matching the RTree module's
/// `rtreeValueUp`.
fn coord_up(d: f64) -> f32 {
    let f = d as f32;
    if f64::from(f) < d {
        (d * if d < 0.0 { RND_TOWARDS } else { RND_AWAY }) as f32
    } else {
        f
    }
}

/// One cell: an id (rowid for a leaf, child node number for an internal node)
/// and its `f32` bounds in the node-blob's coordinate order.
#[derive(Debug, Clone, Copy)]
struct Cell {
    id: i64,
    /// `[min_x, max_x, min_y, max_y]`, the order the RTree stores them in.
    bounds: [f32; 4],
}

impl Cell {
    /// The union of two cells' bounds. Both inputs are already `f32`, so the
    /// union is exact and needs no further rounding.
    fn union(bounds: &[f32; 4], other: &[f32; 4]) -> [f32; 4] {
        [
            bounds[0].min(other[0]),
            bounds[1].max(other[1]),
            bounds[2].min(other[2]),
            bounds[3].max(other[3]),
        ]
    }

    /// The centre of this cell, used for the spatial sort.
    fn centre(&self) -> (f64, f64) {
        (
            (f64::from(self.bounds[0]) + f64::from(self.bounds[1])) / 2.0,
            (f64::from(self.bounds[2]) + f64::from(self.bounds[3])) / 2.0,
        )
    }
}

/// A fully built RTree, as the rows to write into the three shadow tables.
#[derive(Debug)]
pub(crate) struct PackedRtree {
    /// `(nodeno, blob)`, root first.
    pub(crate) nodes: Vec<(i64, Vec<u8>)>,
    /// `(rowid, nodeno)` for every indexed row.
    pub(crate) rowid_map: Vec<(i64, i64)>,
    /// `(nodeno, parentnode)` for every non-root node.
    pub(crate) parent_map: Vec<(i64, i64)>,
}

/// Hilbert index of a point on a 16-bit-per-axis grid over `extent`, used to
/// order entries so that nodes packed from consecutive entries are spatially
/// compact.
///
/// `extent` is the bounding box of the whole entry set rather than a fixed
/// domain, so this works for projected coordinate systems as well as degrees.
fn hilbert(x: f64, y: f64, extent: &[f64; 4]) -> u32 {
    const SIDE: u32 = 1 << 16;
    let norm = |v: f64, lo: f64, hi: f64| -> u32 {
        // A degenerate extent (all centres equal on this axis) or a non-finite
        // coordinate collapses to 0: order is then arbitrary but consistent.
        if !v.is_finite() || hi <= lo {
            return 0;
        }
        // `t` is clamped into [0, 1], so the scaled value is non-negative and
        // at most SIDE - 1.
        let t = ((v - lo) / (hi - lo)).clamp(0.0, 1.0);
        let scaled = t * f64::from(SIDE - 1);
        #[expect(
            clippy::cast_sign_loss,
            reason = "t is clamped to [0, 1], so scaled lies in [0, SIDE - 1] and is never negative"
        )]
        let index = scaled as u32;
        index.min(SIDE - 1)
    };
    let mut hx = norm(x, extent[0], extent[1]);
    let mut hy = norm(y, extent[2], extent[3]);

    let mut d: u32 = 0;
    let mut s: u32 = SIDE / 2;
    while s > 0 {
        let rx = u32::from((hx & s) > 0);
        let ry = u32::from((hy & s) > 0);
        d = d.wrapping_add(s.wrapping_mul(s).wrapping_mul((3 * rx) ^ ry));
        // Rotate the quadrant so the curve stays continuous.
        if ry == 0 {
            if rx == 1 {
                hx = s.wrapping_sub(1).wrapping_sub(hx);
                hy = s.wrapping_sub(1).wrapping_sub(hy);
            }
            std::mem::swap(&mut hx, &mut hy);
        }
        s /= 2;
    }
    d
}

/// Serialise one node: the header followed by its cells, zero-padded to
/// `node_size`.
fn encode_node(depth: u16, cells: &[Cell], node_size: usize) -> Vec<u8> {
    let mut blob = Vec::with_capacity(node_size);
    blob.extend_from_slice(&depth.to_be_bytes());
    // The cell count fits u16: callers cap cells at (node_size - 4) / 24.
    let count = u16::try_from(cells.len()).unwrap_or(u16::MAX);
    blob.extend_from_slice(&count.to_be_bytes());
    for cell in cells {
        blob.extend_from_slice(&cell.id.to_be_bytes());
        for coord in &cell.bounds {
            blob.extend_from_slice(&coord.to_be_bytes());
        }
    }
    blob.resize(node_size, 0);
    blob
}

/// Build an RTree over `entries` (`(rowid, [min_x, max_x, min_y, max_y])` in
/// `f64`) as shadow-table rows.
///
/// `node_size` is the byte length the RTree module expects every node blob to
/// have; read it from the freshly created index rather than re-deriving it, so
/// the two cannot disagree.
///
/// An empty entry set still produces the root node, which the RTree module
/// requires to exist: a leaf at depth 0 with no cells.
pub(crate) fn pack(entries: &[(i64, [f64; 4])], node_size: usize) -> Result<PackedRtree> {
    let capacity = node_size.saturating_sub(NODE_HEADER) / BYTES_PER_CELL;
    debug_assert!(capacity > 0, "node size {node_size} holds no cells");

    // Round each envelope outward to f32, exactly as the RTree module would,
    // so a stored box never excludes a geometry it must contain.
    let mut leaves: Vec<Cell> = entries
        .iter()
        .map(|&(id, [min_x, max_x, min_y, max_y])| Cell {
            id,
            bounds: [
                coord_down(min_x),
                coord_up(max_x),
                coord_down(min_y),
                coord_up(max_y),
            ],
        })
        .collect();

    if leaves.is_empty() {
        return Ok(PackedRtree {
            nodes: vec![(1, encode_node(0, &[], node_size))],
            rowid_map: Vec::new(),
            parent_map: Vec::new(),
        });
    }

    // Group the entries into leaf-sized, spatially compact sets by sorting on
    // the Hilbert index of each centre. Lee and Lee's OMT partitioning was
    // measured against this and is not better here: on uniformly spread data it
    // builds ~15% slower (it sorts at every level rather than once) with
    // slightly worse queries, and on clustered data the two are within noise.
    let extent = leaves.iter().fold(
        [
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ],
        |acc, cell| {
            let (cx, cy) = cell.centre();
            [
                acc[0].min(cx),
                acc[1].max(cx),
                acc[2].min(cy),
                acc[3].max(cy),
            ]
        },
    );
    let mut keyed: Vec<(u32, Cell)> = leaves
        .drain(..)
        .map(|cell| {
            let (cx, cy) = cell.centre();
            (hilbert(cx, cy, &extent), cell)
        })
        .collect();
    keyed.sort_unstable_by_key(|(key, _)| *key);
    let groups = keyed
        .chunks(capacity)
        .map(|chunk| chunk.iter().map(|(_, cell)| *cell).collect::<Vec<Cell>>());

    // Level 0: pack the sorted entries into leaf nodes. Node numbers are
    // assigned provisionally from 2 upward; the root takes 1 at the end.
    let mut next_nodeno: i64 = 2;
    let mut nodes: Vec<(i64, Vec<u8>)> = Vec::new();
    let mut rowid_map: Vec<(i64, i64)> = Vec::new();
    let mut parent_map: Vec<(i64, i64)> = Vec::new();

    // Cells describing the level just built, to be packed by the level above.
    let mut level: Vec<Cell> = Vec::new();
    for cells in groups {
        let nodeno = next_nodeno;
        next_nodeno += 1;
        let mut bounds = cells.first().map_or([0.0; 4], |first| first.bounds);
        for cell in &cells {
            bounds = Cell::union(&bounds, &cell.bounds);
            rowid_map.push((cell.id, nodeno));
        }
        nodes.push((nodeno, encode_node(0, &cells, node_size)));
        level.push(Cell { id: nodeno, bounds });
    }

    // Build internal levels until one node remains.
    let mut depth: u16 = 0;
    while level.len() > 1 {
        depth += 1;
        let mut parents: Vec<Cell> = Vec::new();
        for chunk in level.chunks(capacity) {
            let nodeno = next_nodeno;
            next_nodeno += 1;
            let mut bounds = chunk.first().map_or([0.0; 4], |first| first.bounds);
            for cell in chunk {
                bounds = Cell::union(&bounds, &cell.bounds);
                parent_map.push((cell.id, nodeno));
            }
            nodes.push((nodeno, encode_node(depth, chunk, node_size)));
            parents.push(Cell { id: nodeno, bounds });
        }
        level = parents;
    }

    // The single remaining node is the root and must be node 1. Renumber it,
    // and repoint anything that referred to it by its provisional number.
    let root_provisional = level.first().map_or(1, |cell| cell.id);
    renumber_root(
        root_provisional,
        &mut nodes,
        &mut rowid_map,
        &mut parent_map,
        depth,
        node_size,
    );

    Ok(PackedRtree {
        nodes,
        rowid_map,
        parent_map,
    })
}

/// Give the root node number 1 and stamp the tree depth into its header.
///
/// The root was built with a provisional number like any other node. Only the
/// root carries the depth field, and only cells *inside* nodes reference other
/// nodes, so renumbering means rewriting the root's own entry plus any mapping
/// row that pointed at it.
fn renumber_root(
    provisional: i64,
    nodes: &mut [(i64, Vec<u8>)],
    rowid_map: &mut [(i64, i64)],
    parent_map: &mut [(i64, i64)],
    depth: u16,
    node_size: usize,
) {
    for (nodeno, blob) in nodes.iter_mut() {
        if *nodeno == provisional {
            *nodeno = 1;
            // Re-encode with the depth in the header: the root is the only node
            // whose first two bytes are read.
            let cells = decode_cells(blob);
            *blob = encode_node(depth, &cells, node_size);
        }
    }
    for (_, nodeno) in rowid_map.iter_mut() {
        if *nodeno == provisional {
            *nodeno = 1;
        }
    }
    for (child, parent) in parent_map.iter_mut() {
        if *parent == provisional {
            *parent = 1;
        }
        debug_assert_ne!(*child, 1, "the root must not be anyone's child");
    }
}

/// Read the cells back out of an encoded node, used when the root is re-encoded
/// with its depth.
fn decode_cells(blob: &[u8]) -> Vec<Cell> {
    let count = blob
        .get(2..4)
        .and_then(|b| <[u8; 2]>::try_from(b).ok())
        .map_or(0, |b| usize::from(u16::from_be_bytes(b)));
    let mut cells = Vec::with_capacity(count);
    for i in 0..count {
        let start = NODE_HEADER + i * BYTES_PER_CELL;
        let Some(raw) = blob.get(start..start + BYTES_PER_CELL) else {
            break;
        };
        let id = <[u8; 8]>::try_from(raw.get(0..8).unwrap_or_default())
            .map(i64::from_be_bytes)
            .unwrap_or_default();
        let mut bounds = [0.0f32; 4];
        for (n, slot) in bounds.iter_mut().enumerate() {
            let at = 8 + n * 4;
            *slot = <[u8; 4]>::try_from(raw.get(at..at + 4).unwrap_or_default())
                .map(f32::from_be_bytes)
                .unwrap_or_default();
        }
        cells.push(Cell { id, bounds });
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every node blob is exactly `node_size` bytes, which the RTree module
    /// requires: it derives the node size from the root's length and rejects
    /// any node that differs.
    #[test]
    fn all_nodes_have_the_declared_size() {
        let entries: Vec<(i64, [f64; 4])> = (0..5000)
            .map(|i| {
                let f = f64::from(i);
                (i64::from(i), [f, f + 1.0, -f, -f + 1.0])
            })
            .collect();
        let packed = pack(&entries, 1228).unwrap();
        assert!(packed.nodes.len() > 1, "expected a multi-level tree");
        for (_, blob) in &packed.nodes {
            assert_eq!(blob.len(), 1228);
        }
    }

    /// An empty index still has a root node: a leaf at depth 0 with no cells.
    #[test]
    fn empty_entry_set_still_has_a_root() {
        let packed = pack(&[], 1228).unwrap();
        assert_eq!(packed.nodes.len(), 1);
        let (nodeno, blob) = packed.nodes.first().unwrap();
        assert_eq!(*nodeno, 1);
        assert_eq!(blob.len(), 1228);
        assert_eq!(u16::from_be_bytes([blob[0], blob[1]]), 0, "depth");
        assert_eq!(u16::from_be_bytes([blob[2], blob[3]]), 0, "cell count");
        assert!(packed.rowid_map.is_empty());
        assert!(packed.parent_map.is_empty());
    }

    /// A set small enough for one node produces a single root leaf, and every
    /// rowid maps to it.
    #[test]
    fn small_set_is_a_single_root_leaf() {
        let entries: Vec<(i64, [f64; 4])> = (1..=10)
            .map(|i| (i, [i as f64, i as f64, 0.0, 0.0]))
            .collect();
        let packed = pack(&entries, 1228).unwrap();
        assert_eq!(packed.nodes.len(), 1);
        assert_eq!(packed.nodes.first().unwrap().0, 1);
        assert!(packed.parent_map.is_empty());
        assert_eq!(packed.rowid_map.len(), 10);
        assert!(packed.rowid_map.iter().all(|&(_, nodeno)| nodeno == 1));
    }

    /// The root is node 1 and no node claims it as a child.
    #[test]
    fn root_is_node_one() {
        let entries: Vec<(i64, [f64; 4])> = (0..20_000)
            .map(|i| (i64::from(i), [f64::from(i), f64::from(i), 0.0, 0.0]))
            .collect();
        let packed = pack(&entries, 1228).unwrap();
        assert!(packed.nodes.iter().any(|(nodeno, _)| *nodeno == 1));
        assert!(
            packed.parent_map.iter().all(|&(child, _)| child != 1),
            "the root must not appear as a child"
        );
    }

    /// Minimum bounds round down and maxima round up, so a stored box always
    /// contains the true `f64` envelope.
    #[test]
    fn bounds_are_rounded_outward() {
        // A value with no exact f32 representation.
        let lo = 0.1_f64;
        let hi = 0.300_000_000_000_000_04_f64;
        assert!(f64::from(coord_down(lo)) <= lo);
        assert!(f64::from(coord_up(hi)) >= hi);

        // And across the whole packing path.
        let entries = vec![(1_i64, [lo, hi, -hi, -lo])];
        let packed = pack(&entries, 1228).unwrap();
        let cells = decode_cells(&packed.nodes.first().unwrap().1);
        let cell = cells.first().unwrap();
        assert!(f64::from(cell.bounds[0]) <= lo);
        assert!(f64::from(cell.bounds[1]) >= hi);
        assert!(f64::from(cell.bounds[2]) <= -hi);
        assert!(f64::from(cell.bounds[3]) >= -lo);
    }

    /// Every parent cell's bounds contain the bounds of the node it points at,
    /// which is the containment rule `rtreecheck` enforces.
    #[test]
    fn parent_bounds_contain_child_bounds() {
        let entries: Vec<(i64, [f64; 4])> = (0..30_000)
            .map(|i| {
                let x = f64::from(i % 173) * 1.7;
                let y = f64::from(i % 91) * -2.3;
                (i64::from(i), [x, x + 0.5, y, y + 0.5])
            })
            .collect();
        let packed = pack(&entries, 1228).unwrap();

        let bounds_of: std::collections::HashMap<i64, [f32; 4]> = packed
            .nodes
            .iter()
            .map(|(nodeno, blob)| {
                let cells = decode_cells(blob);
                let mut bounds = cells.first().map_or([0.0; 4], |c| c.bounds);
                for cell in &cells {
                    bounds = Cell::union(&bounds, &cell.bounds);
                }
                (*nodeno, bounds)
            })
            .collect();

        for (nodeno, blob) in &packed.nodes {
            // Only internal nodes have cells pointing at other nodes.
            if packed.parent_map.iter().any(|&(child, _)| child == *nodeno) || *nodeno == 1 {
                for cell in decode_cells(blob) {
                    if let Some(child) = bounds_of.get(&cell.id)
                        && packed
                            .parent_map
                            .iter()
                            .any(|&(c, p)| c == cell.id && p == *nodeno)
                    {
                        assert!(cell.bounds[0] <= child[0], "min_x not contained");
                        assert!(cell.bounds[1] >= child[1], "max_x not contained");
                        assert!(cell.bounds[2] <= child[2], "min_y not contained");
                        assert!(cell.bounds[3] >= child[3], "max_y not contained");
                    }
                }
            }
        }
    }

    /// Every entry appears exactly once in `%_rowid`, and every non-root node
    /// exactly once in `%_parent`: the counts `rtreecheck` compares against.
    #[test]
    fn mappings_cover_every_entry_and_node() {
        let entries: Vec<(i64, [f64; 4])> = (0..12_345)
            .map(|i| (i64::from(i), [f64::from(i), f64::from(i), 0.0, 0.0]))
            .collect();
        let packed = pack(&entries, 1228).unwrap();

        let mut ids: Vec<i64> = packed.rowid_map.iter().map(|&(id, _)| id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), entries.len(), "every rowid mapped exactly once");

        let mut children: Vec<i64> = packed.parent_map.iter().map(|&(c, _)| c).collect();
        children.sort_unstable();
        children.dedup();
        assert_eq!(
            children.len(),
            packed.nodes.len() - 1,
            "every non-root node has exactly one parent entry"
        );
    }
}
