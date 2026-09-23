//! `Snapshot`: an immutable manifest paired with the store root, handed to readers (SPEC §5,
//! §6). Loading one takes no lock; the manifest it wraps never changes underneath it.

use std::path::{Path, PathBuf};

use super::{Manifest, TableName};

#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    manifest: Manifest,
    root: PathBuf,
}

impl Snapshot {
    pub fn new(root: PathBuf, manifest: Manifest) -> Snapshot {
        Snapshot { manifest, root }
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn version(&self) -> u64 {
        self.manifest.version
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether segment `id` of `table` is still live in this snapshot's manifest.
    pub fn names_segment(&self, table: &TableName, id: u64) -> bool {
        self.manifest
            .table(table)
            .is_some_and(|t| t.segments.iter().any(|s| s.id == id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Commit, Edit};

    #[test]
    fn names_segment_reflects_the_wrapped_manifest() {
        let table = TableName::new("d", "t");
        let empty = Manifest::empty();
        let with_table = Commit {
            base: 0,
            edits: vec![Edit::CreateTable {
                name: table.clone(),
                engine: "append".to_string(),
                schema: vec![],
            }],
        }
        .apply(&empty, 0)
        .unwrap();

        let snap = Snapshot::new(PathBuf::from("/root"), with_table);
        assert!(!snap.names_segment(&table, 1));
        assert_eq!(snap.root(), Path::new("/root"));
        assert_eq!(snap.version(), 1);
    }
}
