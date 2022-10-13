use std::mem;

use nom_sql::analysis::visit_mut::VisitorMut;
use nom_sql::{ItemPlaceholder, Literal, SelectStatement};

/// Visitor used to remove and return all literals in the order that that are visited. Removed
/// literals are replaced with [`ItemPlaceholder::QuestionMark`].
struct StripLiteralsVisitor {
    literals: Vec<Literal>,
}

/// [`SelectStatement`] wrapper generated by the StripLiteralsVisitor.
#[allow(dead_code)]
pub struct SelectStatementSkeleton(SelectStatement);

impl StripLiteralsVisitor {
    fn new() -> Self {
        Self {
            literals: Vec::new(),
        }
    }
}

#[allow(dead_code)]
impl SelectStatementSkeleton {
    /// Decompose a [`SelectStatement`] into a [`SelectStatementSkeleton`] and a [`Vec<Literal>`] of
    /// all Literals stripped from the statement.
    ///
    /// This is used to construct a [`SelectStatementSkeleton`].
    pub fn decompose_select(mut stmt: SelectStatement) -> (Self, Vec<Literal>) {
        let literals = stmt.strip_literals();
        (SelectStatementSkeleton(stmt), literals)
    }
}

impl<'ast> VisitorMut<'ast> for StripLiteralsVisitor {
    type Error = !;

    fn visit_literal(&mut self, literal: &'ast mut Literal) -> Result<(), Self::Error> {
        self.literals.push(mem::replace(
            literal,
            Literal::Placeholder(ItemPlaceholder::QuestionMark),
        ));
        Ok(())
    }
}

/// Strips all literals from the expression and replaces them with
/// [`ItemPlaceholder::QuestionMark`]. The stripped literals are returned in the order that they are
/// visited.
pub trait StripLiterals {
    fn strip_literals(&mut self) -> Vec<Literal>;
}

impl StripLiterals for SelectStatement {
    fn strip_literals(&mut self) -> Vec<Literal> {
        let mut visitor = StripLiteralsVisitor::new();
        let Ok(()) = visitor.visit_select_statement(self);
        visitor.literals
    }
}

#[cfg(test)]
mod test {
    use nom_sql::{parse_select_statement, Dialect, ItemPlaceholder, Literal};

    use crate::strip_literals::SelectStatementSkeleton;

    #[test]
    fn strips_all_literals() {
        let select = parse_select_statement(
            Dialect::MySQL,
            "SELECT \"literal\", a FROM t WHERE t.b = 1 OR (2 = 3) LIMIT ? OFFSET ?;",
        )
        .unwrap();
        let (stmt, literals) = SelectStatementSkeleton::decompose_select(select);
        assert_eq!(
            literals,
            vec![
                Literal::String("literal".to_string()),
                Literal::UnsignedInteger(1),
                Literal::UnsignedInteger(2),
                Literal::UnsignedInteger(3),
                Literal::Placeholder(ItemPlaceholder::QuestionMark),
                Literal::Placeholder(ItemPlaceholder::QuestionMark)
            ]
        );
        assert_eq!(
            stmt.0,
            parse_select_statement(
                Dialect::MySQL,
                "SELECT ?, a FROM t WHERE t.b = ? OR (? = ?) LIMIT ? OFFSET ?"
            )
            .unwrap()
        );
    }
}
