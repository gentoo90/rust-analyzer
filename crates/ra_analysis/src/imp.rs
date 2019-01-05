use std::sync::Arc;

use salsa::Database;

use hir::{
    self, FnSignatureInfo, Problem, source_binder,
};
use ra_db::{FilesDatabase, SourceRoot, SourceRootId, SyntaxDatabase};
use ra_editor::{self, find_node_at_offset, assists, LocalEdit, Severity};
use ra_syntax::{
    algo::{find_covering_node, visit::{visitor, Visitor}},
    ast::{self, ArgListOwner, Expr, FnDef, NameOwner},
    AstNode, SourceFileNode,
    SyntaxKind::*,
    SyntaxNode, SyntaxNodeRef, TextRange, TextUnit,
};

use crate::{
    AnalysisChange,
    Cancelable, NavigationTarget,
    CrateId, db, Diagnostic, FileId, FilePosition, FileRange, FileSystemEdit,
    Query, ReferenceResolution, RootChange, SourceChange, SourceFileEdit,
    symbol_index::{LibrarySymbolsQuery, FileSymbol},
};

impl db::RootDatabase {
    pub(crate) fn apply_change(&mut self, change: AnalysisChange) {
        log::info!("apply_change {:?}", change);
        // self.gc_syntax_trees();
        if !change.new_roots.is_empty() {
            let mut local_roots = Vec::clone(&self.local_roots());
            for (root_id, is_local) in change.new_roots {
                self.query_mut(ra_db::SourceRootQuery)
                    .set(root_id, Default::default());
                if is_local {
                    local_roots.push(root_id);
                }
            }
            self.query_mut(ra_db::LocalRootsQuery)
                .set((), Arc::new(local_roots));
        }

        for (root_id, root_change) in change.roots_changed {
            self.apply_root_change(root_id, root_change);
        }
        for (file_id, text) in change.files_changed {
            self.query_mut(ra_db::FileTextQuery).set(file_id, text)
        }
        if !change.libraries_added.is_empty() {
            let mut libraries = Vec::clone(&self.library_roots());
            for library in change.libraries_added {
                libraries.push(library.root_id);
                self.query_mut(ra_db::SourceRootQuery)
                    .set(library.root_id, Default::default());
                self.query_mut(LibrarySymbolsQuery)
                    .set_constant(library.root_id, Arc::new(library.symbol_index));
                self.apply_root_change(library.root_id, library.root_change);
            }
            self.query_mut(ra_db::LibraryRootsQuery)
                .set((), Arc::new(libraries));
        }
        if let Some(crate_graph) = change.crate_graph {
            self.query_mut(ra_db::CrateGraphQuery)
                .set((), Arc::new(crate_graph))
        }
    }

    fn apply_root_change(&mut self, root_id: SourceRootId, root_change: RootChange) {
        let mut source_root = SourceRoot::clone(&self.source_root(root_id));
        for add_file in root_change.added {
            self.query_mut(ra_db::FileTextQuery)
                .set(add_file.file_id, add_file.text);
            self.query_mut(ra_db::FileRelativePathQuery)
                .set(add_file.file_id, add_file.path.clone());
            self.query_mut(ra_db::FileSourceRootQuery)
                .set(add_file.file_id, root_id);
            source_root.files.insert(add_file.path, add_file.file_id);
        }
        for remove_file in root_change.removed {
            self.query_mut(ra_db::FileTextQuery)
                .set(remove_file.file_id, Default::default());
            source_root.files.remove(&remove_file.path);
        }
        self.query_mut(ra_db::SourceRootQuery)
            .set(root_id, Arc::new(source_root));
    }

    #[allow(unused)]
    /// Ideally, we should call this function from time to time to collect heavy
    /// syntax trees. However, if we actually do that, everything is recomputed
    /// for some reason. Needs investigation.
    fn gc_syntax_trees(&mut self) {
        self.query(ra_db::SourceFileQuery)
            .sweep(salsa::SweepStrategy::default().discard_values());
        self.query(hir::db::SourceFileItemsQuery)
            .sweep(salsa::SweepStrategy::default().discard_values());
        self.query(hir::db::FileItemQuery)
            .sweep(salsa::SweepStrategy::default().discard_values());
    }
}

impl db::RootDatabase {
    /// This returns `Vec` because a module may be included from several places. We
    /// don't handle this case yet though, so the Vec has length at most one.
    pub(crate) fn parent_module(
        &self,
        position: FilePosition,
    ) -> Cancelable<Vec<NavigationTarget>> {
        let module = match source_binder::module_from_position(self, position)? {
            None => return Ok(Vec::new()),
            Some(it) => it,
        };
        let (file_id, ast_module) = module.source(self);
        let ast_module = match ast_module {
            None => return Ok(Vec::new()),
            Some(it) => it,
        };
        let ast_module = ast_module.borrowed();
        let name = ast_module.name().unwrap();
        Ok(vec![NavigationTarget {
            file_id,
            name: name.text(),
            range: name.syntax().range(),
            kind: MODULE,
            ptr: None,
        }])
    }
    /// Returns `Vec` for the same reason as `parent_module`
    pub(crate) fn crate_for(&self, file_id: FileId) -> Cancelable<Vec<CrateId>> {
        let module = match source_binder::module_from_file_id(self, file_id)? {
            Some(it) => it,
            None => return Ok(Vec::new()),
        };
        let krate = match module.krate(self)? {
            Some(it) => it,
            None => return Ok(Vec::new()),
        };
        Ok(vec![krate.crate_id()])
    }
    pub(crate) fn approximately_resolve_symbol(
        &self,
        position: FilePosition,
    ) -> Cancelable<Option<ReferenceResolution>> {
        let file = self.source_file(position.file_id);
        let syntax = file.syntax();
        if let Some(name_ref) = find_node_at_offset::<ast::NameRef>(syntax, position.offset) {
            let mut rr = ReferenceResolution::new(name_ref.syntax().range());
            if let Some(fn_descr) =
                source_binder::function_from_child_node(self, position.file_id, name_ref.syntax())?
            {
                let scope = fn_descr.scopes(self);
                // First try to resolve the symbol locally
                if let Some(entry) = scope.resolve_local_name(name_ref) {
                    rr.resolves_to.push(NavigationTarget {
                        file_id: position.file_id,
                        name: entry.name().to_string().into(),
                        range: entry.ptr().range(),
                        kind: NAME,
                        ptr: None,
                    });
                    return Ok(Some(rr));
                };
            }
            // If that fails try the index based approach.
            rr.resolves_to.extend(
                self.index_resolve(name_ref)?
                    .into_iter()
                    .map(NavigationTarget::from_symbol),
            );
            return Ok(Some(rr));
        }
        if let Some(name) = find_node_at_offset::<ast::Name>(syntax, position.offset) {
            let mut rr = ReferenceResolution::new(name.syntax().range());
            if let Some(module) = name.syntax().parent().and_then(ast::Module::cast) {
                if module.has_semi() {
                    if let Some(child_module) =
                        source_binder::module_from_declaration(self, position.file_id, module)?
                    {
                        let file_id = child_module.file_id();
                        let name = match child_module.name() {
                            Some(name) => name.to_string().into(),
                            None => "".into(),
                        };
                        let symbol = NavigationTarget {
                            file_id,
                            name,
                            range: TextRange::offset_len(0.into(), 0.into()),
                            kind: MODULE,
                            ptr: None,
                        };
                        rr.resolves_to.push(symbol);
                        return Ok(Some(rr));
                    }
                }
            }
        }
        Ok(None)
    }

    pub(crate) fn find_all_refs(
        &self,
        position: FilePosition,
    ) -> Cancelable<Vec<(FileId, TextRange)>> {
        let file = self.source_file(position.file_id);
        // Find the binding associated with the offset
        let (binding, descr) = match find_binding(self, &file, position)? {
            None => return Ok(Vec::new()),
            Some(it) => it,
        };

        let mut ret = binding
            .name()
            .into_iter()
            .map(|name| (position.file_id, name.syntax().range()))
            .collect::<Vec<_>>();
        ret.extend(
            descr
                .scopes(self)
                .find_all_refs(binding)
                .into_iter()
                .map(|ref_desc| (position.file_id, ref_desc.range)),
        );

        return Ok(ret);

        fn find_binding<'a>(
            db: &db::RootDatabase,
            source_file: &'a SourceFileNode,
            position: FilePosition,
        ) -> Cancelable<Option<(ast::BindPat<'a>, hir::Function)>> {
            let syntax = source_file.syntax();
            if let Some(binding) = find_node_at_offset::<ast::BindPat>(syntax, position.offset) {
                let descr = ctry!(source_binder::function_from_child_node(
                    db,
                    position.file_id,
                    binding.syntax(),
                )?);
                return Ok(Some((binding, descr)));
            };
            let name_ref = ctry!(find_node_at_offset::<ast::NameRef>(syntax, position.offset));
            let descr = ctry!(source_binder::function_from_child_node(
                db,
                position.file_id,
                name_ref.syntax(),
            )?);
            let scope = descr.scopes(db);
            let resolved = ctry!(scope.resolve_local_name(name_ref));
            let resolved = resolved.ptr().resolve(source_file);
            let binding = ctry!(find_node_at_offset::<ast::BindPat>(
                syntax,
                resolved.range().end()
            ));
            Ok(Some((binding, descr)))
        }
    }
    pub(crate) fn doc_text_for(&self, nav: NavigationTarget) -> Cancelable<Option<String>> {
        let result = match (nav.description(self), nav.docs(self)) {
            (Some(desc), Some(docs)) => {
                Some("```rust\n".to_string() + &*desc + "\n```\n\n" + &*docs)
            }
            (Some(desc), None) => Some("```rust\n".to_string() + &*desc + "\n```"),
            (None, Some(docs)) => Some(docs),
            _ => None,
        };

        Ok(result)
    }

    pub(crate) fn diagnostics(&self, file_id: FileId) -> Cancelable<Vec<Diagnostic>> {
        let syntax = self.source_file(file_id);

        let mut res = ra_editor::diagnostics(&syntax)
            .into_iter()
            .map(|d| Diagnostic {
                range: d.range,
                message: d.msg,
                severity: d.severity,
                fix: d.fix.map(|fix| SourceChange::from_local_edit(file_id, fix)),
            })
            .collect::<Vec<_>>();
        if let Some(m) = source_binder::module_from_file_id(self, file_id)? {
            for (name_node, problem) in m.problems(self) {
                let source_root = self.file_source_root(file_id);
                let diag = match problem {
                    Problem::UnresolvedModule { candidate } => {
                        let create_file = FileSystemEdit::CreateFile {
                            source_root,
                            path: candidate.clone(),
                        };
                        let fix = SourceChange {
                            label: "create module".to_string(),
                            source_file_edits: Vec::new(),
                            file_system_edits: vec![create_file],
                            cursor_position: None,
                        };
                        Diagnostic {
                            range: name_node.range(),
                            message: "unresolved module".to_string(),
                            severity: Severity::Error,
                            fix: Some(fix),
                        }
                    }
                    Problem::NotDirOwner { move_to, candidate } => {
                        let move_file = FileSystemEdit::MoveFile {
                            src: file_id,
                            dst_source_root: source_root,
                            dst_path: move_to.clone(),
                        };
                        let create_file = FileSystemEdit::CreateFile {
                            source_root,
                            path: move_to.join(candidate),
                        };
                        let fix = SourceChange {
                            label: "move file and create module".to_string(),
                            source_file_edits: Vec::new(),
                            file_system_edits: vec![move_file, create_file],
                            cursor_position: None,
                        };
                        Diagnostic {
                            range: name_node.range(),
                            message: "can't declare module at this location".to_string(),
                            severity: Severity::Error,
                            fix: Some(fix),
                        }
                    }
                };
                res.push(diag)
            }
        };
        Ok(res)
    }

    pub(crate) fn assists(&self, frange: FileRange) -> Vec<SourceChange> {
        let file = self.source_file(frange.file_id);
        assists::assists(&file, frange.range)
            .into_iter()
            .map(|local_edit| SourceChange::from_local_edit(frange.file_id, local_edit))
            .collect()
    }

    pub(crate) fn resolve_callable(
        &self,
        position: FilePosition,
    ) -> Cancelable<Option<(FnSignatureInfo, Option<usize>)>> {
        let file = self.source_file(position.file_id);
        let syntax = file.syntax();

        // Find the calling expression and it's NameRef
        let calling_node = ctry!(FnCallNode::with_node(syntax, position.offset));
        let name_ref = ctry!(calling_node.name_ref());

        // Resolve the function's NameRef (NOTE: this isn't entirely accurate).
        let file_symbols = self.index_resolve(name_ref)?;
        for symbol in file_symbols {
            if symbol.ptr.kind() == FN_DEF {
                let fn_file = self.source_file(symbol.file_id);
                let fn_def = symbol.ptr.resolve(&fn_file);
                let fn_def = ast::FnDef::cast(fn_def.borrowed()).unwrap();
                let descr = ctry!(source_binder::function_from_source(
                    self,
                    symbol.file_id,
                    fn_def
                )?);
                if let Some(descriptor) = descr.signature_info(self) {
                    // If we have a calling expression let's find which argument we are on
                    let mut current_parameter = None;

                    let num_params = descriptor.params.len();
                    let has_self = fn_def.param_list().and_then(|l| l.self_param()).is_some();

                    if num_params == 1 {
                        if !has_self {
                            current_parameter = Some(0);
                        }
                    } else if num_params > 1 {
                        // Count how many parameters into the call we are.
                        // TODO: This is best effort for now and should be fixed at some point.
                        // It may be better to see where we are in the arg_list and then check
                        // where offset is in that list (or beyond).
                        // Revisit this after we get documentation comments in.
                        if let Some(ref arg_list) = calling_node.arg_list() {
                            let start = arg_list.syntax().range().start();

                            let range_search = TextRange::from_to(start, position.offset);
                            let mut commas: usize = arg_list
                                .syntax()
                                .text()
                                .slice(range_search)
                                .to_string()
                                .matches(',')
                                .count();

                            // If we have a method call eat the first param since it's just self.
                            if has_self {
                                commas += 1;
                            }

                            current_parameter = Some(commas);
                        }
                    }

                    return Ok(Some((descriptor, current_parameter)));
                }
            }
        }

        Ok(None)
    }

    pub(crate) fn type_of(&self, frange: FileRange) -> Cancelable<Option<String>> {
        let file = self.source_file(frange.file_id);
        let syntax = file.syntax();
        let node = find_covering_node(syntax, frange.range);
        let parent_fn = ctry!(node.ancestors().find_map(FnDef::cast));
        let function = ctry!(source_binder::function_from_source(
            self,
            frange.file_id,
            parent_fn
        )?);
        let infer = function.infer(self)?;
        Ok(infer.type_of_node(node).map(|t| t.to_string()))
    }
    pub(crate) fn rename(
        &self,
        position: FilePosition,
        new_name: &str,
    ) -> Cancelable<Vec<SourceFileEdit>> {
        let res = self
            .find_all_refs(position)?
            .iter()
            .map(|(file_id, text_range)| SourceFileEdit {
                file_id: *file_id,
                edit: {
                    let mut builder = ra_text_edit::TextEditBuilder::default();
                    builder.replace(*text_range, new_name.into());
                    builder.finish()
                },
            })
            .collect::<Vec<_>>();
        Ok(res)
    }
    fn index_resolve(&self, name_ref: ast::NameRef) -> Cancelable<Vec<FileSymbol>> {
        let name = name_ref.text();
        let mut query = Query::new(name.to_string());
        query.exact();
        query.limit(4);
        crate::symbol_index::world_symbols(self, query)
    }
}

impl SourceChange {
    pub(crate) fn from_local_edit(file_id: FileId, edit: LocalEdit) -> SourceChange {
        let file_edit = SourceFileEdit {
            file_id,
            edit: edit.edit,
        };
        SourceChange {
            label: edit.label,
            source_file_edits: vec![file_edit],
            file_system_edits: vec![],
            cursor_position: edit
                .cursor_position
                .map(|offset| FilePosition { offset, file_id }),
        }
    }
}

enum FnCallNode<'a> {
    CallExpr(ast::CallExpr<'a>),
    MethodCallExpr(ast::MethodCallExpr<'a>),
}

impl<'a> FnCallNode<'a> {
    pub fn with_node(syntax: SyntaxNodeRef, offset: TextUnit) -> Option<FnCallNode> {
        if let Some(expr) = find_node_at_offset::<ast::CallExpr>(syntax, offset) {
            return Some(FnCallNode::CallExpr(expr));
        }
        if let Some(expr) = find_node_at_offset::<ast::MethodCallExpr>(syntax, offset) {
            return Some(FnCallNode::MethodCallExpr(expr));
        }
        None
    }

    pub fn name_ref(&self) -> Option<ast::NameRef> {
        match *self {
            FnCallNode::CallExpr(call_expr) => Some(match call_expr.expr()? {
                Expr::PathExpr(path_expr) => path_expr.path()?.segment()?.name_ref()?,
                _ => return None,
            }),

            FnCallNode::MethodCallExpr(call_expr) => call_expr
                .syntax()
                .children()
                .filter_map(ast::NameRef::cast)
                .nth(0),
        }
    }

    pub fn arg_list(&self) -> Option<ast::ArgList> {
        match *self {
            FnCallNode::CallExpr(expr) => expr.arg_list(),
            FnCallNode::MethodCallExpr(expr) => expr.arg_list(),
        }
    }
}

impl NavigationTarget {
    fn node(&self, db: &db::RootDatabase) -> Option<SyntaxNode> {
        let source_file = db.source_file(self.file_id);
        let source_file = source_file.syntax();
        let node = source_file
            .descendants()
            .find(|node| node.kind() == self.kind && node.range() == self.range)?
            .owned();
        Some(node)
    }

    fn docs(&self, db: &db::RootDatabase) -> Option<String> {
        let node = self.node(db)?;
        let node = node.borrowed();
        fn doc_comments<'a, N: ast::DocCommentsOwner<'a>>(node: N) -> Option<String> {
            let comments = node.doc_comment_text();
            if comments.is_empty() {
                None
            } else {
                Some(comments)
            }
        }

        visitor()
            .visit(doc_comments::<ast::FnDef>)
            .visit(doc_comments::<ast::StructDef>)
            .visit(doc_comments::<ast::EnumDef>)
            .visit(doc_comments::<ast::TraitDef>)
            .visit(doc_comments::<ast::Module>)
            .visit(doc_comments::<ast::TypeDef>)
            .visit(doc_comments::<ast::ConstDef>)
            .visit(doc_comments::<ast::StaticDef>)
            .accept(node)?
    }

    /// Get a description of this node.
    ///
    /// e.g. `struct Name`, `enum Name`, `fn Name`
    fn description(&self, db: &db::RootDatabase) -> Option<String> {
        // TODO: After type inference is done, add type information to improve the output
        let node = self.node(db)?;
        let node = node.borrowed();
        // TODO: Refactor to be have less repetition
        visitor()
            .visit(|node: ast::FnDef| {
                let mut string = "fn ".to_string();
                node.name()?.syntax().text().push_to(&mut string);
                Some(string)
            })
            .visit(|node: ast::StructDef| {
                let mut string = "struct ".to_string();
                node.name()?.syntax().text().push_to(&mut string);
                Some(string)
            })
            .visit(|node: ast::EnumDef| {
                let mut string = "enum ".to_string();
                node.name()?.syntax().text().push_to(&mut string);
                Some(string)
            })
            .visit(|node: ast::TraitDef| {
                let mut string = "trait ".to_string();
                node.name()?.syntax().text().push_to(&mut string);
                Some(string)
            })
            .visit(|node: ast::Module| {
                let mut string = "mod ".to_string();
                node.name()?.syntax().text().push_to(&mut string);
                Some(string)
            })
            .visit(|node: ast::TypeDef| {
                let mut string = "type ".to_string();
                node.name()?.syntax().text().push_to(&mut string);
                Some(string)
            })
            .visit(|node: ast::ConstDef| {
                let mut string = "const ".to_string();
                node.name()?.syntax().text().push_to(&mut string);
                Some(string)
            })
            .visit(|node: ast::StaticDef| {
                let mut string = "static ".to_string();
                node.name()?.syntax().text().push_to(&mut string);
                Some(string)
            })
            .accept(node)?
    }
}
