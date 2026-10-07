//! Planning of SELECT: scopes, select items, grouping, ORDER BY resolution
//! and the operator pipeline
//! `source → joins → WHERE → [aggregate → HAVING] → sort → project →
//! DISTINCT → LIMIT`.

use std::iter;

use cairn_sql::{
    BinaryOp, Expr, ExprKind, JoinKind, Literal, Name, Select, SelectItem, Spanned, TableRef,
};

use crate::bind::{
    BoundExpr, BoundKind, CmpOp, Grouping, Mode, Scope, bind, bind_condition, contains_aggregate,
};
use crate::catalog::{Catalog, TableDef};
use crate::error::{ErrorKind, ExecError};
use crate::plan::{Access, Plan, Probe, TableAccess, choose_access, conjuncts};

/// A planned SELECT and its output column names.
pub struct Planned {
    pub plan: Plan,
    pub columns: Vec<String>,
}

struct Source {
    table: TableDef,
    label: String,
}

pub fn plan_select(select: &Select, catalog: &Catalog) -> Result<Planned, ExecError> {
    let mut scope = Scope::default();
    let mut sources = Vec::new();
    if let Some(from) = &select.from {
        let refs =
            iter::once(&from.node.table).chain(from.node.joins.iter().map(|j| &j.node.table));
        for table_ref in refs {
            sources.push(add_source(catalog, table_ref, &mut scope)?);
        }
    }
    let where_bound = match &select.where_clause {
        Some(w) => Some(bind_condition(
            w,
            &scope,
            &mut Mode::Plain("WHERE"),
            "WHERE",
        )?),
        None => None,
    };
    let mut plan = source_plan(catalog, &sources, where_bound.as_ref())?;
    if let Some(from) = &select.from {
        for (i, join) in from.node.joins.iter().enumerate() {
            plan = plan_join(
                catalog,
                plan,
                &scope,
                &sources,
                i + 1,
                &join.node.on,
                join.node.kind,
            )?;
        }
    }
    if let (Some(cond), Some(ast)) = (where_bound, &select.where_clause) {
        plan = Plan::Filter {
            input: Box::new(plan),
            cond,
            text: ast.to_string(),
        };
    }
    finish_select(select, &scope, plan)
}

fn add_source(
    catalog: &Catalog,
    table_ref: &Spanned<TableRef>,
    scope: &mut Scope,
) -> Result<Source, ExecError> {
    let name = &table_ref.node.name;
    let table = catalog.tables.get(&name.node.0).cloned().ok_or_else(|| {
        ExecError::at(
            ErrorKind::NotFound,
            format!("no such table: {}", name.node.0),
            name.span,
        )
    })?;
    let binding = table_ref
        .node
        .alias
        .as_ref()
        .map_or(&name.node.0, |a| &a.node.0)
        .clone();
    if scope.bindings.iter().any(|b| b.name == binding) {
        return Err(ExecError::at(
            ErrorKind::AlreadyExists,
            format!("table name {binding} is used more than once in FROM"),
            table_ref.span,
        ));
    }
    let label = match &table_ref.node.alias {
        Some(alias) => format!("{} AS {}", name.node, alias.node),
        None => name.node.to_string(),
    };
    let columns = table
        .columns
        .iter()
        .map(|c| (c.name.clone(), c.ty))
        .collect();
    scope.push(binding, columns);
    Ok(Source { table, label })
}

fn source_plan(
    catalog: &Catalog,
    sources: &[Source],
    where_bound: Option<&BoundExpr>,
) -> Result<Plan, ExecError> {
    let Some(first) = sources.first() else {
        return Ok(Plan::Values);
    };
    let conds = where_bound.map(conjuncts).unwrap_or_default();
    let indexes = catalog.indexes_of(&first.table.name);
    let access = choose_access(&first.table, &indexes, &conds, 0)?;
    Ok(Plan::Access(TableAccess {
        table: first.table.clone(),
        label: first.label.clone(),
        access,
    }))
}

fn plan_join(
    catalog: &Catalog,
    outer: Plan,
    scope: &Scope,
    sources: &[Source],
    position: usize,
    on_ast: &Expr,
    kind: JoinKind,
) -> Result<Plan, ExecError> {
    let prefix = scope.prefix(position + 1);
    let on = bind_condition(on_ast, &prefix, &mut Mode::Plain("ON"), "ON")?;
    let (Some(source), Some(binding)) = (sources.get(position), scope.bindings.get(position))
    else {
        return Err(ExecError::corrupt("join source out of range"));
    };
    let left = kind == JoinKind::Left;
    if let Some(probe) = find_probe(catalog, &source.table, binding.offset, &on, on_ast) {
        return Ok(Plan::IndexJoin {
            left,
            outer: Box::new(outer),
            table: source.table.clone(),
            label: source.label.clone(),
            probe,
            on,
        });
    }
    let inner = Plan::Access(TableAccess {
        table: source.table.clone(),
        label: source.label.clone(),
        access: Access::Scan,
    });
    Ok(Plan::NestedLoop {
        left,
        outer: Box::new(outer),
        inner: Box::new(inner),
        on,
        inner_width: source.table.columns.len(),
    })
}

fn ast_conjuncts(expr: &Expr) -> Vec<&Expr> {
    match &expr.node {
        ExprKind::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            let mut all = ast_conjuncts(left);
            all.extend(ast_conjuncts(right));
            all
        }
        _ => vec![expr],
    }
}

/// Finds an ON conjunct `inner.col = expr(outer columns)` where `inner.col`
/// is the inner table's INTEGER PRIMARY KEY or has an index and the types
/// are equal.
fn find_probe(
    catalog: &Catalog,
    table: &TableDef,
    offset: usize,
    on: &BoundExpr,
    on_ast: &Expr,
) -> Option<Probe> {
    let width = table.columns.len();
    let indexes = catalog.indexes_of(&table.name);
    for (bound, ast) in conjuncts(on).into_iter().zip(ast_conjuncts(on_ast)) {
        let (BoundKind::Compare(CmpOp::Eq, l, r), ExprKind::Binary { left, right, .. }) =
            (&bound.kind, &ast.node)
        else {
            continue;
        };
        for (column_side, other, other_ast) in [(l, r, right), (r, l, left)] {
            let BoundKind::Column(c) = column_side.kind else {
                continue;
            };
            if c < offset || c >= offset + width {
                continue;
            }
            if other.max_column().is_some_and(|m| m >= offset) {
                continue;
            }
            let ordinal = c - offset;
            let Some(column) = table.columns.get(ordinal) else {
                continue;
            };
            if other.ty != column.ty {
                continue;
            }
            let index = if table.pk == Some(ordinal) {
                None
            } else {
                Some(indexes.iter().find(|i| i.column == ordinal)?.clone())
            };
            return Some(Probe {
                index,
                expr: (**other).clone(),
                text: format!("{} = {}", Name(column.name.clone()), other_ast),
            });
        }
    }
    None
}

/// One output item before binding: its expression and output name.
struct Item {
    expr: Expr,
    name: String,
    alias: Option<String>,
}

fn expand_items(select: &Select, scope: &Scope) -> Result<Vec<Item>, ExecError> {
    let mut items = Vec::new();
    for item in &select.items {
        match &item.node {
            SelectItem::Wildcard => {
                if scope.bindings.is_empty() {
                    return Err(ExecError::at(
                        ErrorKind::Type,
                        "SELECT * requires a FROM clause",
                        item.span,
                    ));
                }
                for binding in &scope.bindings {
                    expand_binding(&mut items, &binding.name, &binding.columns, item.span);
                }
            }
            SelectItem::QualifiedWildcard(qualifier) => {
                let binding = scope
                    .bindings
                    .iter()
                    .find(|b| b.name == qualifier.node.0)
                    .ok_or_else(|| {
                        ExecError::at(
                            ErrorKind::NotFound,
                            format!("no such table or alias: {}", qualifier.node.0),
                            qualifier.span,
                        )
                    })?;
                expand_binding(&mut items, &binding.name, &binding.columns, item.span);
            }
            SelectItem::Expr { expr, alias } => {
                let alias = alias.as_ref().map(|a| a.node.0.clone());
                let name = match (&alias, &expr.node) {
                    (Some(alias), _) => alias.clone(),
                    (None, ExprKind::Column { name, .. }) => name.node.0.clone(),
                    (None, _) => expr.to_string(),
                };
                items.push(Item {
                    expr: expr.clone(),
                    name,
                    alias,
                });
            }
        }
    }
    Ok(items)
}

fn expand_binding(
    items: &mut Vec<Item>,
    binding: &str,
    columns: &[(String, crate::value::SqlType)],
    span: cairn_sql::Span,
) {
    for (column, _) in columns {
        let ident = |name: &str| Spanned::new(Name(name.to_string()), span);
        let expr = Spanned::new(
            ExprKind::Column {
                table: Some(ident(binding)),
                name: ident(column),
            },
            span,
        );
        items.push(Item {
            expr,
            name: column.clone(),
            alias: None,
        });
    }
}

fn is_aggregating(select: &Select) -> bool {
    !select.group_by.is_empty()
        || select.having.is_some()
        || select.items.iter().any(|item| match &item.node {
            SelectItem::Expr { expr, .. } => contains_aggregate(expr),
            _ => false,
        })
        || select
            .order_by
            .iter()
            .any(|o| contains_aggregate(&o.node.expr))
}

fn finish_select(select: &Select, scope: &Scope, mut plan: Plan) -> Result<Planned, ExecError> {
    let items = expand_items(select, scope)?;
    let aggregating = is_aggregating(select);
    let mut grouping = Grouping::default();
    let mut group_bound = Vec::with_capacity(select.group_by.len());
    for group in &select.group_by {
        let bound = bind(group, scope, &mut Mode::Plain("GROUP BY"))?;
        grouping.groups.push((group.clone(), bound.ty));
        group_bound.push(bound);
    }
    let mut mode = if aggregating {
        Mode::Grouped(&mut grouping)
    } else {
        Mode::Plain("SELECT")
    };
    let mut exprs = Vec::with_capacity(items.len());
    for item in &items {
        exprs.push(bind(&item.expr, scope, &mut mode)?);
    }
    let having = match &select.having {
        Some(h) => Some(bind_condition(h, scope, &mut mode, "HAVING")?),
        None => None,
    };
    let keys = order_keys(select, scope, &mut mode, &items, &exprs)?;
    if aggregating {
        let text = if select.group_by.is_empty() {
            String::new()
        } else {
            format!("group by {}", join_display(&select.group_by))
        };
        plan = Plan::Aggregate {
            input: Box::new(plan),
            groups: group_bound,
            aggs: grouping.aggs,
            text,
        };
        if let (Some(cond), Some(ast)) = (having, &select.having) {
            plan = Plan::Filter {
                input: Box::new(plan),
                cond,
                text: ast.to_string(),
            };
        }
    }
    if !keys.is_empty() {
        let text = select
            .order_by
            .iter()
            .map(|o| {
                let suffix = if o.node.descending { " DESC" } else { "" };
                format!("{}{suffix}", o.node.expr)
            })
            .collect::<Vec<_>>()
            .join(", ");
        plan = Plan::Sort {
            input: Box::new(plan),
            keys,
            text,
        };
    }
    let names: Vec<String> = items.into_iter().map(|i| i.name).collect();
    plan = Plan::Project {
        input: Box::new(plan),
        exprs,
        names: names.clone(),
    };
    if select.distinct {
        plan = Plan::Distinct {
            input: Box::new(plan),
        };
    }
    if let Some(limit) = &select.limit {
        plan = Plan::Limit {
            input: Box::new(plan),
            count: limit.node.count.node,
            offset: limit.node.offset.as_ref().map_or(0, |o| o.node),
        };
    }
    Ok(Planned {
        plan,
        columns: names,
    })
}

fn join_display(exprs: &[Expr]) -> String {
    exprs
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Resolves ORDER BY items: an integer literal is an output position, a
/// bare name equal to an output alias is that item, anything else is bound
/// like a select item. With DISTINCT every key must be a select item.
fn order_keys(
    select: &Select,
    scope: &Scope,
    mode: &mut Mode,
    items: &[Item],
    exprs: &[BoundExpr],
) -> Result<Vec<(BoundExpr, bool)>, ExecError> {
    let mut keys = Vec::with_capacity(select.order_by.len());
    for order in &select.order_by {
        let expr = &order.node.expr;
        let (bound, is_item) = match &expr.node {
            ExprKind::Literal(Literal::Integer(n)) => {
                let position = usize::try_from(*n)
                    .ok()
                    .filter(|p| *p >= 1 && *p <= exprs.len());
                let bound = position.and_then(|p| exprs.get(p - 1)).ok_or_else(|| {
                    ExecError::at(
                        ErrorKind::Type,
                        format!(
                            "ORDER BY position {n} is out of range (1 to {})",
                            exprs.len()
                        ),
                        expr.span,
                    )
                })?;
                (bound.clone(), true)
            }
            ExprKind::Column { table: None, name }
                if items
                    .iter()
                    .any(|i| i.alias.as_deref() == Some(name.node.0.as_str())) =>
            {
                let index = items
                    .iter()
                    .position(|i| i.alias.as_deref() == Some(name.node.0.as_str()));
                let bound = index.and_then(|i| exprs.get(i)).cloned();
                (
                    bound.ok_or_else(|| ExecError::corrupt("alias out of range"))?,
                    true,
                )
            }
            _ => {
                let bound = bind(expr, scope, mode)?;
                let same_ast = items.iter().any(|i| i.expr == *expr);
                let same_column = match bound.kind {
                    BoundKind::Column(c) => exprs
                        .iter()
                        .any(|e| matches!(e.kind, BoundKind::Column(d) if d == c)),
                    _ => false,
                };
                (bound, same_ast || same_column)
            }
        };
        if select.distinct && !is_item {
            return Err(ExecError::at(
                ErrorKind::Type,
                "with SELECT DISTINCT, ORDER BY expressions must appear in the select list",
                order.span,
            ));
        }
        keys.push((bound, order.node.descending));
    }
    Ok(keys)
}
