//! ra_db defines basic database traits. Concrete DB is defined by ra_analysis.
mod cancelation;
mod syntax_ptr;
mod input;
mod loc2id;
pub mod mock;

use std::sync::Arc;

use ra_editor::LineIndex;
use ra_syntax::{TextUnit, TextRange, SourceFileNode};

pub use crate::{
    cancelation::{Canceled, Cancelable},
    syntax_ptr::LocalSyntaxPtr,
    input::{
        FilesDatabase, FileId, CrateId, SourceRoot, SourceRootId, CrateGraph, Dependency,
        FileTextQuery, FileSourceRootQuery, SourceRootQuery, LocalRootsQuery, LibraryRootsQuery, CrateGraphQuery,
        FileRelativePathQuery
    },
    loc2id::{LocationIntener, NumericId},
};

#[macro_export]
macro_rules! impl_numeric_id {
    ($id:ident) => {
        impl $crate::NumericId for $id {
            fn from_u32(id: u32) -> Self {
                $id(id)
            }
            fn to_u32(self) -> u32 {
                self.0
            }
        }
    };
}

pub trait BaseDatabase: salsa::Database {
    fn check_canceled(&self) -> Cancelable<()> {
        if self.salsa_runtime().is_current_revision_canceled() {
            Err(Canceled::new())
        } else {
            Ok(())
        }
    }
}

salsa::query_group! {
    pub trait SyntaxDatabase: crate::input::FilesDatabase + BaseDatabase {
        fn source_file(file_id: FileId) -> SourceFileNode {
            type SourceFileQuery;
        }
        fn file_lines(file_id: FileId) -> Arc<LineIndex> {
            type FileLinesQuery;
        }
    }
}

fn source_file(db: &impl SyntaxDatabase, file_id: FileId) -> SourceFileNode {
    let text = db.file_text(file_id);
    SourceFileNode::parse(&*text)
}
fn file_lines(db: &impl SyntaxDatabase, file_id: FileId) -> Arc<LineIndex> {
    let text = db.file_text(file_id);
    Arc::new(LineIndex::new(&*text))
}

#[derive(Clone, Copy, Debug)]
pub struct FilePosition {
    pub file_id: FileId,
    pub offset: TextUnit,
}

#[derive(Clone, Copy, Debug)]
pub struct FileRange {
    pub file_id: FileId,
    pub range: TextRange,
}
