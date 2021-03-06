use ra_db::{Cancelable, SyntaxDatabase};
use ra_syntax::{
    AstNode, SyntaxNode,
    ast::{self, NameOwner},
    algo::{find_covering_node, visit::{visitor, Visitor}},
};

use crate::{db::RootDatabase, RangeInfo, FilePosition, FileRange, NavigationTarget};

pub(crate) fn hover(
    db: &RootDatabase,
    position: FilePosition,
) -> Cancelable<Option<RangeInfo<String>>> {
    let mut res = Vec::new();
    let range = if let Some(rr) = db.approximately_resolve_symbol(position)? {
        for nav in rr.resolves_to {
            res.extend(doc_text_for(db, nav)?)
        }
        rr.reference_range
    } else {
        let file = db.source_file(position.file_id);
        let expr: ast::Expr = ctry!(ra_editor::find_node_at_offset(
            file.syntax(),
            position.offset
        ));
        let frange = FileRange {
            file_id: position.file_id,
            range: expr.syntax().range(),
        };
        res.extend(type_of(db, frange)?);
        expr.syntax().range()
    };
    if res.is_empty() {
        return Ok(None);
    }
    let res = RangeInfo::new(range, res.join("\n\n---\n"));
    Ok(Some(res))
}

pub(crate) fn type_of(db: &RootDatabase, frange: FileRange) -> Cancelable<Option<String>> {
    let file = db.source_file(frange.file_id);
    let syntax = file.syntax();
    let node = find_covering_node(syntax, frange.range);
    let parent_fn = ctry!(node.ancestors().find_map(ast::FnDef::cast));
    let function = ctry!(hir::source_binder::function_from_source(
        db,
        frange.file_id,
        parent_fn
    )?);
    let infer = function.infer(db)?;
    Ok(infer.type_of_node(node).map(|t| t.to_string()))
}

// FIXME: this should not really use navigation target. Rather, approximatelly
// resovled symbol should return a `DefId`.
fn doc_text_for(db: &RootDatabase, nav: NavigationTarget) -> Cancelable<Option<String>> {
    let result = match (nav.description(db), nav.docs(db)) {
        (Some(desc), Some(docs)) => Some("```rust\n".to_string() + &*desc + "\n```\n\n" + &*docs),
        (Some(desc), None) => Some("```rust\n".to_string() + &*desc + "\n```"),
        (None, Some(docs)) => Some(docs),
        _ => None,
    };

    Ok(result)
}

impl NavigationTarget {
    fn node(&self, db: &RootDatabase) -> Option<SyntaxNode> {
        let source_file = db.source_file(self.file_id);
        let source_file = source_file.syntax();
        let node = source_file
            .descendants()
            .find(|node| node.kind() == self.kind && node.range() == self.range)?
            .owned();
        Some(node)
    }

    fn docs(&self, db: &RootDatabase) -> Option<String> {
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
    fn description(&self, db: &RootDatabase) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use ra_syntax::TextRange;

    use crate::mock_analysis::single_file_with_position;

    #[test]
    fn hover_shows_type_of_an_expression() {
        let (analysis, position) = single_file_with_position(
            "
            pub fn foo() -> u32 { 1 }

            fn main() {
                let foo_test = foo()<|>;
            }
        ",
        );
        let hover = analysis.hover(position).unwrap().unwrap();
        assert_eq!(hover.range, TextRange::from_to(95.into(), 100.into()));
        assert_eq!(hover.info, "u32");
    }
}
