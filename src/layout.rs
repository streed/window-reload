//! Reconstruct a workspace's dwindle tiling tree (a binary space partition) from
//! the saved pixel geometry of its windows.
//!
//! dwindle layouts are guillotine partitions: at every level the region is cut by
//! a single straight vertical or horizontal line into two sub-regions. We recover
//! that structure so restore can replay the splits in the right order and with the
//! right orientation.

use crate::model::Rect;

/// Orientation of a split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orient {
    /// A vertical divider: children sit side by side (left | right).
    Vertical,
    /// A horizontal divider: children are stacked (top / bottom).
    Horizontal,
}

/// A node in the reconstructed tiling tree. Leaf values are indices into the
/// caller's window slice.
#[derive(Debug, Clone)]
pub enum Node {
    Leaf(usize),
    Split {
        orient: Orient,
        first: Box<Node>,
        second: Box<Node>,
    },
}

impl Node {
    /// The index reached by always descending into the first child. Under dwindle
    /// this is the window that "anchors" the subtree (the earliest-spawned one).
    pub fn first_leaf(&self) -> usize {
        match self {
            Node::Leaf(i) => *i,
            Node::Split { first, .. } => first.first_leaf(),
        }
    }

    /// All leaf indices, in left-to-right / top-to-bottom order.
    pub fn leaves(&self) -> Vec<usize> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves(&self, out: &mut Vec<usize>) {
        match self {
            Node::Leaf(i) => out.push(*i),
            Node::Split { first, second, .. } => {
                first.collect_leaves(out);
                second.collect_leaves(out);
            }
        }
    }
}

/// Slack (pixels) allowed when deciding whether a straight cut cleanly separates
/// two groups. Covers Hyprland's gaps and borders.
const TOL: i64 = 40;

/// An indexed rectangle (window index paired with its geometry).
type Item = (usize, Rect);

/// The two sides of a guillotine cut plus its orientation.
type Cut = (Vec<Item>, Vec<Item>, Orient);

/// Reconstruct the tiling tree for a set of windows.
///
/// `items` pairs each window's caller-side index with its rectangle. The result
/// is a binary tree whose leaves are those indices.
pub fn reconstruct(items: &[(usize, Rect)]) -> Node {
    if items.len() == 1 {
        return Node::Leaf(items[0].0);
    }
    if let Some((left, right, orient)) = guillotine(items) {
        return Node::Split {
            orient,
            first: Box::new(reconstruct(&left)),
            second: Box::new(reconstruct(&right)),
        };
    }
    // Degenerate fallback (overlapping/odd geometry): split the sorted list in half
    // so we still produce a usable order rather than failing.
    let mut sorted = items.to_vec();
    sorted.sort_by_key(|(_, r)| (r.x, r.y));
    let mid = sorted.len() / 2;
    Node::Split {
        orient: Orient::Vertical,
        first: Box::new(reconstruct(&sorted[..mid])),
        second: Box::new(reconstruct(&sorted[mid..])),
    }
}

/// Find the outermost clean guillotine cut. Prefers the leftmost vertical cut,
/// then the topmost horizontal cut.
fn guillotine(items: &[Item]) -> Option<Cut> {
    // Vertical cut: sort by x; the first k form the left group iff their right
    // edges all lie left of the remaining windows' left edges.
    let mut by_x = items.to_vec();
    by_x.sort_by_key(|(_, r)| r.x);
    for k in 1..by_x.len() {
        let left_max = by_x[..k].iter().map(|(_, r)| r.x + r.w).max().unwrap();
        let right_min = by_x[k..].iter().map(|(_, r)| r.x).min().unwrap();
        if left_max <= right_min + TOL {
            return Some((by_x[..k].to_vec(), by_x[k..].to_vec(), Orient::Vertical));
        }
    }

    // Horizontal cut: sort by y; likewise on the vertical axis.
    let mut by_y = items.to_vec();
    by_y.sort_by_key(|(_, r)| r.y);
    for k in 1..by_y.len() {
        let top_max = by_y[..k].iter().map(|(_, r)| r.y + r.h).max().unwrap();
        let bottom_min = by_y[k..].iter().map(|(_, r)| r.y).min().unwrap();
        if top_max <= bottom_min + TOL {
            return Some((by_y[..k].to_vec(), by_y[k..].to_vec(), Orient::Horizontal));
        }
    }
    None
}

/// Determine the actual orientation between two live rectangles: are they mostly
/// separated horizontally (side by side) or vertically (stacked)?
pub fn actual_orientation(a: Rect, b: Rect) -> Orient {
    let ac = (a.x + a.w / 2, a.y + a.h / 2);
    let bc = (b.x + b.w / 2, b.y + b.h / 2);
    if (ac.0 - bc.0).abs() >= (ac.1 - bc.1).abs() {
        Orient::Vertical
    } else {
        Orient::Horizontal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i64, y: i64, w: i64, h: i64) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn single() {
        let t = reconstruct(&[(7, r(0, 0, 100, 100))]);
        assert_eq!(t.leaves(), vec![7]);
    }

    #[test]
    fn two_columns() {
        // side by side
        let t = reconstruct(&[(0, r(0, 0, 50, 100)), (1, r(52, 0, 48, 100))]);
        match t {
            Node::Split { orient, .. } => assert_eq!(orient, Orient::Vertical),
            _ => panic!("expected split"),
        }
        assert_eq!(t.leaves(), vec![0, 1]);
        assert_eq!(t.first_leaf(), 0);
    }

    #[test]
    fn two_rows() {
        let t = reconstruct(&[(0, r(0, 0, 100, 50)), (1, r(0, 52, 100, 48))]);
        match t {
            Node::Split { orient, .. } => assert_eq!(orient, Orient::Horizontal),
            _ => panic!("expected split"),
        }
    }

    #[test]
    fn column_of_three() {
        // left column full height, right column split into two rows
        let items = [
            (0, r(0, 0, 50, 100)),
            (1, r(52, 0, 48, 50)),
            (2, r(52, 52, 48, 48)),
        ];
        let t = reconstruct(&items);
        // Outer cut is vertical; leaves preserve left-to-right, top-to-bottom order.
        assert_eq!(t.leaves(), vec![0, 1, 2]);
        assert_eq!(t.first_leaf(), 0);
    }

    #[test]
    fn grid_2x2() {
        let items = [
            (0, r(0, 0, 50, 50)),
            (1, r(52, 0, 48, 50)),
            (2, r(0, 52, 50, 48)),
            (3, r(52, 52, 48, 48)),
        ];
        let t = reconstruct(&items);
        let mut leaves = t.leaves();
        leaves.sort();
        assert_eq!(leaves, vec![0, 1, 2, 3]);
    }
}
