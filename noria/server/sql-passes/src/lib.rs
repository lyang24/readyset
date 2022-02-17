#![feature(box_patterns, result_flattening)]

pub mod alias_removal;
mod count_star_rewrite;
mod detect_problematic_self_joins;
mod implied_tables;
mod key_def_coalescing;
mod negation_removal;
mod normalize_topk_with_aggregate;
mod order_limit_removal;
mod rewrite_between;
mod star_expansion;
mod strip_post_filters;
pub mod subqueries;

use std::collections::HashMap;
use std::convert::TryFrom;

pub use nom_sql::analysis::{contains_aggregate, is_aggregate};
use nom_sql::{
    BinaryOperator, Column, CommonTableExpression, Expression, FieldDefinitionExpression,
    FunctionExpression, InValue, JoinClause, JoinRightSide, LimitClause, Literal, SelectStatement,
};
use noria_errors::{unsupported, ReadySetResult};

pub use crate::alias_removal::AliasRemoval;
pub use crate::count_star_rewrite::CountStarRewrite;
pub use crate::detect_problematic_self_joins::DetectProblematicSelfJoins;
pub use crate::implied_tables::ImpliedTableExpansion;
pub use crate::key_def_coalescing::KeyDefinitionCoalescing;
pub use crate::negation_removal::NegationRemoval;
pub use crate::normalize_topk_with_aggregate::NormalizeTopKWithAggregate;
pub use crate::order_limit_removal::OrderLimitRemoval;
pub use crate::rewrite_between::RewriteBetween;
pub use crate::star_expansion::StarExpansion;
pub use crate::strip_post_filters::StripPostFilters;
pub use crate::subqueries::SubQueries;

fn field_names(statement: &SelectStatement) -> impl Iterator<Item = &str> {
    statement.fields.iter().filter_map(|field| match &field {
        FieldDefinitionExpression::Expression {
            alias: Some(alias), ..
        } => Some(alias.as_str()),
        FieldDefinitionExpression::Expression {
            expr: Expression::Column(Column { name, .. }),
            ..
        } => Some(name.as_str()),
        _ => None,
    })
}

/// Returns a map from subquery aliases to vectors of the fields in those subqueries.
///
/// Takes only the CTEs and join clause so that it doesn't have to borrow the entire statement.
pub(self) fn subquery_schemas<'a>(
    ctes: &'a [CommonTableExpression],
    join: &'a [JoinClause],
) -> HashMap<&'a str, Vec<&'a str>> {
    ctes.iter()
        .map(|cte| (cte.name.as_str(), &cte.statement))
        .chain(join.iter().filter_map(|join| match &join.right {
            JoinRightSide::NestedSelect(stmt, Some(name)) => Some((name.as_str(), stmt.as_ref())),
            _ => None,
        }))
        .map(|(name, stmt)| (name, field_names(stmt).collect()))
        .collect()
}

#[must_use]
pub fn map_aggregates(expr: &mut Expression) -> Vec<(FunctionExpression, String)> {
    let mut ret = Vec::new();
    match expr {
        Expression::Call(f) if is_aggregate(f) => {
            let name = f.to_string();
            ret.push((f.clone(), name.clone()));
            *expr = Expression::Column(Column {
                name,
                table: None,
                function: None,
            });
        }
        Expression::CaseWhen {
            condition,
            then_expr,
            else_expr,
        } => {
            ret.append(&mut map_aggregates(condition));
            ret.append(&mut map_aggregates(then_expr));
            if let Some(else_expr) = else_expr {
                ret.append(&mut map_aggregates(else_expr));
            }
        }
        Expression::Call(_)
        | Expression::Literal(_)
        | Expression::Column(_)
        | Expression::Variable(_) => {}
        Expression::BinaryOp { lhs, rhs, .. } => {
            ret.append(&mut map_aggregates(lhs));
            ret.append(&mut map_aggregates(rhs));
        }
        Expression::UnaryOp { rhs: expr, .. } | Expression::Cast { expr, .. } => {
            ret.append(&mut map_aggregates(expr));
        }
        Expression::Exists(_) => {}
        Expression::NestedSelect(_) => {}
        Expression::Between {
            operand, min, max, ..
        } => {
            ret.append(&mut map_aggregates(operand));
            ret.append(&mut map_aggregates(min));
            ret.append(&mut map_aggregates(max));
        }
        Expression::In { lhs, rhs, .. } => {
            ret.append(&mut map_aggregates(lhs));
            match rhs {
                InValue::Subquery(_) => {}
                InValue::List(exprs) => {
                    for expr in exprs {
                        ret.append(&mut map_aggregates(expr));
                    }
                }
            }
        }
    }
    ret
}

/// Returns true if the given binary operator is a (boolean-valued) predicate
///
/// TODO(grfn): Replace this with actual typechecking at some point
pub fn is_predicate(op: &BinaryOperator) -> bool {
    use BinaryOperator::*;

    matches!(
        op,
        Like | NotLike
            | ILike
            | NotILike
            | Equal
            | NotEqual
            | Greater
            | GreaterOrEqual
            | Less
            | LessOrEqual
            | Is
            | IsNot
    )
}

/// Returns true if the given binary operator is a (boolean-valued) logical operator
///
/// TODO(grfn): Replace this with actual typechecking at some point
pub fn is_logical_op(op: &BinaryOperator) -> bool {
    use BinaryOperator::*;

    matches!(op, And | Or)
}

/// Boolean-valued logical operators
pub enum LogicalOp {
    And,
    Or,
}

impl TryFrom<BinaryOperator> for LogicalOp {
    type Error = BinaryOperator;

    fn try_from(value: BinaryOperator) -> Result<Self, Self::Error> {
        match value {
            BinaryOperator::And => Ok(Self::And),
            BinaryOperator::Or => Ok(Self::Or),
            _ => Err(value),
        }
    }
}

pub fn extract_limit(limit: &LimitClause) -> ReadySetResult<usize> {
    if let Expression::Literal(Literal::Integer(k)) = limit.limit {
        if k < 0 {
            unsupported!("Invalid value for limit: {}", k);
        }
        Ok(k as usize)
    } else {
        unsupported!(
            "Only literal limits are supported ({} supplied)",
            limit.limit
        );
    }
}