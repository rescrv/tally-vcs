//! Comparing two states, path by path.
//!
//! A flat path→blob map makes the difference of two states an O(paths)
//! merge, no tree walk: the shared primitive under `status` (working tree
//! vs. a ref) and `diff` (any two refs).  Because the states are setsums,
//! the group difference of their sums is itself a verifiable checksum of the
//! symmetric difference — a fingerprint of exactly what changed, computed
//! without inspecting a single byte of content.

use crate::ident::{ElementRecord, Sum};
use crate::manifest::Manifest;

/// One path's change between two states.  `before` absent is an addition;
/// `after` absent is a removal; both present is a modification (blob or mode).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathChange {
    /// The element path.
    pub path: String,
    /// The record in the earlier state, if the path existed there.
    pub before: Option<ElementRecord>,
    /// The record in the later state, if the path exists there.
    pub after: Option<ElementRecord>,
}

impl PathChange {
    /// A single-character status code, git-style: `A` added, `D` deleted,
    /// `M` modified.
    pub fn code(&self) -> char {
        match (&self.before, &self.after) {
            (None, Some(_)) => 'A',
            (Some(_), None) => 'D',
            _ => 'M',
        }
    }
}

/// The changes taking state `before` to state `after`, one per differing
/// path, in bytewise path order.  A path present in both with an identical
/// record is not a change and does not appear.
pub fn diff_manifests(before: &Manifest, after: &Manifest) -> Vec<PathChange> {
    let mut paths: Vec<&str> = Vec::new();
    for r in before.records() {
        paths.push(&r.path);
    }
    for r in after.records() {
        paths.push(&r.path);
    }
    paths.sort_unstable();
    paths.dedup();
    let mut changes = Vec::new();
    for path in paths {
        let b = before.get(path);
        let a = after.get(path);
        if a == b {
            continue;
        }
        changes.push(PathChange {
            path: path.to_string(),
            before: b.cloned(),
            after: a.cloned(),
        });
    }
    changes
}

/// The group difference of two states' sums: `after - before`.  This is a
/// setsum fingerprint of the symmetric difference — zero exactly when the
/// states are identical, and independent of the order changes were made.
pub fn pending_sum(before: &Sum, after: &Sum) -> Sum {
    after.clone() - before.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ident::sha3_hex;

    fn rec(mode: &str, path: &str, content: &[u8]) -> ElementRecord {
        ElementRecord::new(mode, path, &sha3_hex(content)).unwrap()
    }

    #[test]
    fn classifies_add_delete_modify() {
        let before = Manifest::from_records([
            rec("100644", "/keep", b"same"),
            rec("100644", "/gone", b"x"),
            rec("100644", "/edit", b"v1"),
        ])
        .unwrap();
        let after = Manifest::from_records([
            rec("100644", "/keep", b"same"),
            rec("100644", "/edit", b"v2"),
            rec("100644", "/new", b"y"),
        ])
        .unwrap();
        let changes = diff_manifests(&before, &after);
        let codes: Vec<(char, &str)> =
            changes.iter().map(|c| (c.code(), c.path.as_str())).collect();
        // Bytewise path order: /edit, /gone, /new.  /keep is unchanged.
        assert_eq!(codes, vec![('M', "/edit"), ('D', "/gone"), ('A', "/new")]);
    }

    #[test]
    fn mode_only_change_is_a_modification() {
        let before = Manifest::from_records([rec("100644", "/t", b"x")]).unwrap();
        let after = Manifest::from_records([rec("100755", "/t", b"x")]).unwrap();
        let changes = diff_manifests(&before, &after);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].code(), 'M');
    }

    #[test]
    fn pending_sum_is_zero_iff_identical() {
        let a = Manifest::from_records([rec("100644", "/a", b"a")]).unwrap();
        let b = Manifest::from_records([rec("100644", "/a", b"a")]).unwrap();
        assert_eq!(pending_sum(&a.sum(), &b.sum()), Sum::zero());
        let c = Manifest::from_records([rec("100644", "/a", b"different")]).unwrap();
        assert_ne!(pending_sum(&a.sum(), &c.sum()), Sum::zero());
    }
}
