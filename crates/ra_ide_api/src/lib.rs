//! ra_ide_api crate provides "ide-centric" APIs for the rust-analyzer. That is,
//! it generally operates with files and text ranges, and returns results as
//! Strings, suitable for displaying to the human.
//!
//! What powers this API are the `RootDatabase` struct, which defines a `salsa`
//! database, and the `ra_hir` crate, where majority of the analysis happens.
//! However, IDE specific bits of the analysis (most notably completion) happen
//! in this crate.
//!
//! The sibling `ra_ide_api_light` handles thouse bits of IDE functionality
//! which are restricted to a single file and need only syntax.

// For proving that RootDatabase is RefUnwindSafe.
#![recursion_limit = "128"]

mod db;
mod imp;
pub mod mock_analysis;
mod symbol_index;
mod navigation_target;
mod change;

mod status;
mod completion;
mod runnables;
mod goto_definition;
mod extend_selection;
mod hover;
mod call_info;
mod syntax_highlighting;
mod parent_module;
mod rename;
mod impls;
mod assists;

#[cfg(test)]
mod marks;

use std::sync::Arc;

use ra_syntax::{SourceFile, TreeArc, TextRange, TextUnit};
use ra_text_edit::TextEdit;
use ra_db::{
    SourceDatabase, CheckCanceled,
    salsa::{self, ParallelDatabase},
};
use relative_path::RelativePathBuf;

use crate::{
    symbol_index::FileSymbol,
    db::LineIndexDatabase,
};

pub use crate::{
    completion::{CompletionItem, CompletionItemKind, InsertTextFormat},
    runnables::{Runnable, RunnableKind},
    navigation_target::NavigationTarget,
    change::{AnalysisChange, LibraryData},
};
pub use ra_ide_api_light::{
    Fold, FoldKind, HighlightedRange, Severity, StructureNode,
    LineIndex, LineCol, translate_offset_with_edit,
};
pub use ra_db::{
    Canceled, CrateGraph, CrateId, FileId, FilePosition, FileRange, SourceRootId
};
pub use hir::Documentation;

// We use jemalloc mainly to get heap usage statistics, actual performance
// differnece is not measures.
#[cfg(feature = "jemalloc")]
#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

pub type Cancelable<T> = Result<T, Canceled>;

#[derive(Debug)]
pub struct SourceChange {
    pub label: String,
    pub source_file_edits: Vec<SourceFileEdit>,
    pub file_system_edits: Vec<FileSystemEdit>,
    pub cursor_position: Option<FilePosition>,
}

#[derive(Debug)]
pub struct SourceFileEdit {
    pub file_id: FileId,
    pub edit: TextEdit,
}

#[derive(Debug)]
pub enum FileSystemEdit {
    CreateFile {
        source_root: SourceRootId,
        path: RelativePathBuf,
    },
    MoveFile {
        src: FileId,
        dst_source_root: SourceRootId,
        dst_path: RelativePathBuf,
    },
}

#[derive(Debug)]
pub struct Diagnostic {
    pub message: String,
    pub range: TextRange,
    pub fix: Option<SourceChange>,
    pub severity: Severity,
}

#[derive(Debug)]
pub struct Query {
    query: String,
    lowercased: String,
    only_types: bool,
    libs: bool,
    exact: bool,
    limit: usize,
}

impl Query {
    pub fn new(query: String) -> Query {
        let lowercased = query.to_lowercase();
        Query {
            query,
            lowercased,
            only_types: false,
            libs: false,
            exact: false,
            limit: usize::max_value(),
        }
    }

    pub fn only_types(&mut self) {
        self.only_types = true;
    }

    pub fn libs(&mut self) {
        self.libs = true;
    }

    pub fn exact(&mut self) {
        self.exact = true;
    }

    pub fn limit(&mut self, limit: usize) {
        self.limit = limit
    }
}

#[derive(Debug)]
pub struct RangeInfo<T> {
    pub range: TextRange,
    pub info: T,
}

impl<T> RangeInfo<T> {
    pub fn new(range: TextRange, info: T) -> RangeInfo<T> {
        RangeInfo { range, info }
    }
}

#[derive(Debug)]
pub struct CallInfo {
    pub label: String,
    pub doc: Option<Documentation>,
    pub parameters: Vec<String>,
    pub active_parameter: Option<usize>,
}

/// `AnalysisHost` stores the current state of the world.
#[derive(Debug, Default)]
pub struct AnalysisHost {
    db: db::RootDatabase,
}

impl AnalysisHost {
    /// Returns a snapshot of the current state, which you can query for
    /// semantic information.
    pub fn analysis(&self) -> Analysis {
        Analysis {
            db: self.db.snapshot(),
        }
    }

    /// Applies changes to the current state of the world. If there are
    /// outstanding snapshots, they will be canceled.
    pub fn apply_change(&mut self, change: AnalysisChange) {
        self.db.apply_change(change)
    }

    pub fn maybe_collect_garbage(&mut self) {
        self.db.maybe_collect_garbage();
    }

    pub fn collect_garbage(&mut self) {
        self.db.collect_garbage();
    }
}

/// Analysis is a snapshot of a world state at a moment in time. It is the main
/// entry point for asking semantic information about the world. When the world
/// state is advanced using `AnalysisHost::apply_change` method, all existing
/// `Analysis` are canceled (most method return `Err(Canceled)`).
#[derive(Debug)]
pub struct Analysis {
    db: salsa::Snapshot<db::RootDatabase>,
}

impl Analysis {
    /// Debug info about the current state of the analysis
    pub fn status(&self) -> String {
        status::status(&*self.db)
    }

    /// Gets the text of the source file.
    pub fn file_text(&self, file_id: FileId) -> Arc<String> {
        self.db.file_text(file_id)
    }

    /// Gets the syntax tree of the file.
    pub fn parse(&self, file_id: FileId) -> TreeArc<SourceFile> {
        self.db.parse(file_id).clone()
    }

    /// Gets the file's `LineIndex`: data structure to convert between absolute
    /// offsets and line/column representation.
    pub fn file_line_index(&self, file_id: FileId) -> Arc<LineIndex> {
        self.db.line_index(file_id)
    }

    /// Selects the next syntactic nodes encopasing the range.
    pub fn extend_selection(&self, frange: FileRange) -> Cancelable<TextRange> {
        self.with_db(|db| extend_selection::extend_selection(db, frange))
    }

    /// Returns position of the mathcing brace (all types of braces are
    /// supported).
    pub fn matching_brace(&self, position: FilePosition) -> Option<TextUnit> {
        let file = self.db.parse(position.file_id);
        ra_ide_api_light::matching_brace(&file, position.offset)
    }

    /// Returns a syntax tree represented as `String`, for debug purposes.
    // FIXME: use a better name here.
    pub fn syntax_tree(&self, file_id: FileId) -> String {
        let file = self.db.parse(file_id);
        ra_ide_api_light::syntax_tree(&file)
    }

    /// Returns an edit to remove all newlines in the range, cleaning up minor
    /// stuff like trailing commas.
    pub fn join_lines(&self, frange: FileRange) -> SourceChange {
        let file = self.db.parse(frange.file_id);
        SourceChange::from_local_edit(
            frange.file_id,
            ra_ide_api_light::join_lines(&file, frange.range),
        )
    }

    /// Returns an edit which should be applied when opening a new line, fixing
    /// up minor stuff like continuing the comment.
    pub fn on_enter(&self, position: FilePosition) -> Option<SourceChange> {
        let file = self.db.parse(position.file_id);
        let edit = ra_ide_api_light::on_enter(&file, position.offset)?;
        Some(SourceChange::from_local_edit(position.file_id, edit))
    }

    /// Returns an edit which should be applied after `=` was typed. Primarily,
    /// this works when adding `let =`.
    // FIXME: use a snippet completion instead of this hack here.
    pub fn on_eq_typed(&self, position: FilePosition) -> Option<SourceChange> {
        let file = self.db.parse(position.file_id);
        let edit = ra_ide_api_light::on_eq_typed(&file, position.offset)?;
        Some(SourceChange::from_local_edit(position.file_id, edit))
    }

    /// Returns an edit which should be applied when a dot ('.') is typed on a blank line, indenting the line appropriately.
    pub fn on_dot_typed(&self, position: FilePosition) -> Option<SourceChange> {
        let file = self.db.parse(position.file_id);
        let edit = ra_ide_api_light::on_dot_typed(&file, position.offset)?;
        Some(SourceChange::from_local_edit(position.file_id, edit))
    }

    /// Returns a tree representation of symbols in the file. Useful to draw a
    /// file outline.
    pub fn file_structure(&self, file_id: FileId) -> Vec<StructureNode> {
        let file = self.db.parse(file_id);
        ra_ide_api_light::file_structure(&file)
    }

    /// Returns the set of folding ranges.
    pub fn folding_ranges(&self, file_id: FileId) -> Vec<Fold> {
        let file = self.db.parse(file_id);
        ra_ide_api_light::folding_ranges(&file)
    }

    /// Fuzzy searches for a symbol.
    pub fn symbol_search(&self, query: Query) -> Cancelable<Vec<NavigationTarget>> {
        self.with_db(|db| {
            symbol_index::world_symbols(db, query)
                .into_iter()
                .map(NavigationTarget::from_symbol)
                .collect::<Vec<_>>()
        })
    }

    pub fn goto_definition(
        &self,
        position: FilePosition,
    ) -> Cancelable<Option<RangeInfo<Vec<NavigationTarget>>>> {
        self.with_db(|db| goto_definition::goto_definition(db, position))
    }

    pub fn goto_implementation(
        &self,
        position: FilePosition,
    ) -> Cancelable<Option<RangeInfo<Vec<NavigationTarget>>>> {
        self.with_db(|db| impls::goto_implementation(db, position))
    }

    /// Finds all usages of the reference at point.
    pub fn find_all_refs(&self, position: FilePosition) -> Cancelable<Vec<(FileId, TextRange)>> {
        self.with_db(|db| db.find_all_refs(position))
    }

    /// Returns a short text descrbing element at position.
    pub fn hover(&self, position: FilePosition) -> Cancelable<Option<RangeInfo<String>>> {
        self.with_db(|db| hover::hover(db, position))
    }

    /// Computes parameter information for the given call expression.
    pub fn call_info(&self, position: FilePosition) -> Cancelable<Option<CallInfo>> {
        self.with_db(|db| call_info::call_info(db, position))
    }

    /// Returns a `mod name;` declaration which created the current module.
    pub fn parent_module(&self, position: FilePosition) -> Cancelable<Vec<NavigationTarget>> {
        self.with_db(|db| parent_module::parent_module(db, position))
    }

    /// Returns crates this file belongs too.
    pub fn crate_for(&self, file_id: FileId) -> Cancelable<Vec<CrateId>> {
        self.with_db(|db| db.crate_for(file_id))
    }

    /// Returns the root file of the given crate.
    pub fn crate_root(&self, crate_id: CrateId) -> Cancelable<FileId> {
        self.with_db(|db| db.crate_graph().crate_root(crate_id))
    }

    /// Returns the set of possible targets to run for the current file.
    pub fn runnables(&self, file_id: FileId) -> Cancelable<Vec<Runnable>> {
        self.with_db(|db| runnables::runnables(db, file_id))
    }

    /// Computes syntax highlighting for the given file.
    pub fn highlight(&self, file_id: FileId) -> Cancelable<Vec<HighlightedRange>> {
        self.with_db(|db| syntax_highlighting::highlight(db, file_id))
    }

    /// Computes completions at the given position.
    pub fn completions(&self, position: FilePosition) -> Cancelable<Option<Vec<CompletionItem>>> {
        self.with_db(|db| completion::completions(db, position).map(Into::into))
    }

    /// Computes assists (aks code actons aka intentions) for the given
    /// position.
    pub fn assists(&self, frange: FileRange) -> Cancelable<Vec<SourceChange>> {
        self.with_db(|db| assists::assists(db, frange))
    }

    /// Computes the set of diagnostics for the given file.
    pub fn diagnostics(&self, file_id: FileId) -> Cancelable<Vec<Diagnostic>> {
        self.with_db(|db| db.diagnostics(file_id))
    }

    /// Computes the type of the expression at the given position.
    pub fn type_of(&self, frange: FileRange) -> Cancelable<Option<String>> {
        self.with_db(|db| hover::type_of(db, frange))
    }

    /// Returns the edit required to rename reference at the position to the new
    /// name.
    pub fn rename(
        &self,
        position: FilePosition,
        new_name: &str,
    ) -> Cancelable<Option<SourceChange>> {
        self.with_db(|db| rename::rename(db, position, new_name))
    }

    fn with_db<F: FnOnce(&db::RootDatabase) -> T + std::panic::UnwindSafe, T>(
        &self,
        f: F,
    ) -> Cancelable<T> {
        self.db.catch_canceled(f)
    }
}

#[test]
fn analysis_is_send() {
    fn is_send<T: Send>() {}
    is_send::<Analysis>();
}
