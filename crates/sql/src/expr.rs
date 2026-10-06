//! Pratt parser for expressions.
//!
//! Levels follow the milestone precedence table (see `ast::precedence`).
//! Prefix operators are strict: `NOT` is accepted only where a level-3
//! expression may start, so `a = NOT b` must be written `a = (NOT b)`.
//! Likewise an operator binding tighter than a predicate cannot follow one,
//! so `a IS NULL || b` must be written `(a IS NULL) || b`. Under these rules
//! a child sits below the level of its position only where the source had
//! parentheses, which is what lets `Display` print minimal parentheses.
//!
//! Every recursive call goes through [`Parser::enter`], which bounds both
//! the nesting depth and the number of nodes under construction, so the
//! recursion depth is bounded no matter what the input is.

use crate::MAX_DEPTH;
use crate::ast::{
    BinaryOp, CaseBranch, Expr, ExprKind, FunctionArgs, Ident, Literal, Name, Spanned, UnaryOp,
    precedence,
};
use crate::parser::{Descent, ParseResult, Parser};
use crate::span::Span;
use crate::token::{Keyword, TokenKind};

/// A boxed expression and the height of its tree (0 for a leaf). Boxing
/// keeps results small, which keeps the recursive stack frames small.
struct Parsed {
    expr: Box<Expr>,
    height: usize,
}

impl Parser<'_> {
    pub(crate) fn expr(&mut self) -> ParseResult<Expr> {
        Ok(*self.expr_bp(precedence::OR)?.expr)
    }

    /// Parses an expression whose operators all bind at `min` or tighter.
    fn expr_bp(&mut self, min: u8) -> ParseResult<Parsed> {
        let (mut left, mut left_level) = self.prefix(min)?;
        loop {
            let Some(level) = self.infix_level() else {
                return Ok(left);
            };
            if level < min || level > left_level {
                return Ok(left);
            }
            left = if level == precedence::PREDICATE {
                self.predicate(left)?
            } else {
                self.binary(left)?
            };
            left_level = level;
        }
    }

    fn infix_level(&self) -> Option<u8> {
        if let Some(op) = binary_op(&self.peek().kind) {
            return Some(op.precedence());
        }
        match self.peek_keyword() {
            Some(Keyword::Is | Keyword::Not | Keyword::Between | Keyword::In | Keyword::Like) => {
                Some(precedence::PREDICATE)
            }
            _ => None,
        }
    }

    /// Returns the operand and the level it binds at.
    fn prefix(&mut self, min: u8) -> ParseResult<(Parsed, u8)> {
        let op = match self.peek().kind {
            TokenKind::Keyword(Keyword::Not) if min <= precedence::NOT => UnaryOp::Not,
            TokenKind::Minus if min <= precedence::NEGATION => UnaryOp::Neg,
            _ => return Ok((self.primary()?, precedence::PRIMARY)),
        };
        let level = match op {
            UnaryOp::Not => precedence::NOT,
            UnaryOp::Neg => precedence::NEGATION,
        };
        Ok((self.unary(op, level)?, level))
    }

    fn unary(&mut self, op: UnaryOp, level: u8) -> ParseResult<Parsed> {
        let op_span = self.bump();
        let operand = self.operand(Descent::NestedNode, op_span, level)?;
        let span = op_span.until(operand.expr.span);
        let kind = ExprKind::Unary {
            op,
            operand: operand.expr,
        };
        self.node(kind, span, operand.height + 1, op_span)
    }

    fn binary(&mut self, left: Parsed) -> ParseResult<Parsed> {
        let Some(op) = binary_op(&self.peek().kind) else {
            return Err(self.expected("operator", ""));
        };
        let op_span = self.bump();
        let right = self.operand(Descent::Node, op_span, op.precedence() + 1)?;
        let span = left.expr.span.until(right.expr.span);
        let height = left.height.max(right.height) + 1;
        let kind = ExprKind::Binary {
            op,
            left: left.expr,
            right: right.expr,
        };
        self.node(kind, span, height, op_span)
    }

    /// Parses a child expression at `min` inside the construct opened at `at`.
    fn operand(&mut self, descent: Descent, at: Span, min: u8) -> ParseResult<Parsed> {
        self.enter(descent, at)?;
        let operand = self.expr_bp(min)?;
        self.leave(descent);
        Ok(operand)
    }

    fn predicate(&mut self, subject: Parsed) -> ParseResult<Parsed> {
        let start = self.peek().span;
        if self.eat_keyword(Keyword::Is).is_some() {
            return self.is_null(subject, start);
        }
        let negated = self.eat_keyword(Keyword::Not).is_some();
        match self.peek_keyword() {
            Some(Keyword::Between) => self.between(subject, negated, start),
            Some(Keyword::In) => self.in_list(subject, negated, start),
            Some(Keyword::Like) => self.like(subject, negated, start),
            _ => Err(self.expected("BETWEEN, IN or LIKE", "after NOT")),
        }
    }

    fn is_null(&mut self, subject: Parsed, at: Span) -> ParseResult<Parsed> {
        let negated = self.eat_keyword(Keyword::Not).is_some();
        if self.eat_keyword(Keyword::Null).is_none() {
            return Err(match negated {
                true => self.expected("NULL", "after IS NOT"),
                false => self.expected("NULL or NOT", "after IS"),
            });
        }
        let span = subject.expr.span.until(self.last());
        let kind = ExprKind::IsNull {
            operand: subject.expr,
            negated,
        };
        self.node(kind, span, subject.height + 1, at)
    }

    /// The bounds bind at the `||` level, so the `AND` between them is not logical AND.
    fn between(&mut self, subject: Parsed, negated: bool, at: Span) -> ParseResult<Parsed> {
        self.bump();
        let low = self.operand(Descent::Node, at, precedence::CONCAT)?;
        self.expect_keyword(Keyword::And, "after BETWEEN lower bound")?;
        let high = self.operand(Descent::Node, at, precedence::CONCAT)?;
        let span = subject.expr.span.until(high.expr.span);
        let height = subject.height.max(low.height).max(high.height) + 1;
        let kind = ExprKind::Between {
            operand: subject.expr,
            low: low.expr,
            high: high.expr,
            negated,
        };
        self.node(kind, span, height, at)
    }

    fn in_list(&mut self, subject: Parsed, negated: bool, at: Span) -> ParseResult<Parsed> {
        self.bump();
        let open = self.expect(&TokenKind::LParen, "'('", "after IN")?;
        self.enter(Descent::NestedNode, open)?;
        let (list, list_height) = self.expr_list_with_height()?;
        let close = self.expect(&TokenKind::RParen, "')'", "after IN list")?;
        self.leave(Descent::NestedNode);
        let span = subject.expr.span.until(close);
        let height = subject.height.max(list_height) + 1;
        let kind = ExprKind::InList {
            operand: subject.expr,
            list,
            negated,
        };
        self.node(kind, span, height, at)
    }

    fn like(&mut self, subject: Parsed, negated: bool, at: Span) -> ParseResult<Parsed> {
        self.bump();
        let pattern = self.operand(Descent::Node, at, precedence::CONCAT)?;
        let span = subject.expr.span.until(pattern.expr.span);
        let height = subject.height.max(pattern.height) + 1;
        let kind = ExprKind::Like {
            operand: subject.expr,
            pattern: pattern.expr,
            negated,
        };
        self.node(kind, span, height, at)
    }

    fn primary(&mut self) -> ParseResult<Parsed> {
        let literal = match &self.peek().kind {
            TokenKind::Integer(value) => Literal::Integer(*value),
            TokenKind::Real(value) => Literal::Real(*value),
            TokenKind::String(text) => Literal::String(text.clone()),
            TokenKind::Keyword(Keyword::Null) => Literal::Null,
            TokenKind::Keyword(Keyword::True) => Literal::Boolean(true),
            TokenKind::Keyword(Keyword::False) => Literal::Boolean(false),
            TokenKind::LParen => return self.group(),
            TokenKind::Keyword(Keyword::Case) => return self.case(),
            TokenKind::Ident(name) => {
                let name = Name(name.clone());
                return self.column_or_function(name);
            }
            _ => return Err(self.expected("expression", "")),
        };
        let span = self.bump();
        Ok(leaf(ExprKind::Literal(literal), span))
    }

    /// Parentheses are not tree nodes; they only widen the inner span.
    fn group(&mut self) -> ParseResult<Parsed> {
        let open = self.bump();
        let mut inner = self.operand(Descent::Group, open, precedence::OR)?;
        let close = self.expect(&TokenKind::RParen, "')'", "after expression")?;
        inner.expr.span = open.until(close);
        Ok(inner)
    }

    fn case(&mut self) -> ParseResult<Parsed> {
        let case_span = self.bump();
        self.enter(Descent::NestedNode, case_span)?;
        let mut when_span = self.expect_keyword(Keyword::When, "after CASE")?;
        let mut branches = Vec::new();
        let mut height = 0;
        let else_result = loop {
            let condition = self.expr_bp(precedence::OR)?;
            self.expect_keyword(Keyword::Then, "after WHEN condition")?;
            let result = self.expr_bp(precedence::OR)?;
            height = height.max(condition.height).max(result.height);
            let span = when_span.until(result.expr.span);
            let branch = CaseBranch {
                condition: *condition.expr,
                result: *result.expr,
            };
            branches.push(Spanned::new(branch, span));
            if let Some(next_when) = self.eat_keyword(Keyword::When) {
                when_span = next_when;
                continue;
            }
            if self.eat_keyword(Keyword::Else).is_some() {
                let else_result = self.expr_bp(precedence::OR)?;
                height = height.max(else_result.height);
                self.expect_keyword(Keyword::End, "after ELSE result")?;
                break Some(else_result.expr);
            }
            if self.eat_keyword(Keyword::End).is_some() {
                break None;
            }
            return Err(self.expected("WHEN, ELSE or END", "in CASE"));
        };
        self.leave(Descent::NestedNode);
        let span = case_span.until(self.last());
        let kind = ExprKind::Case {
            branches,
            else_result,
        };
        self.node(kind, span, height + 1, case_span)
    }

    fn column_or_function(&mut self, name: Name) -> ParseResult<Parsed> {
        let name = Spanned::new(name, self.bump());
        if self.at(&TokenKind::LParen) {
            return self.function(name);
        }
        if self.eat(&TokenKind::Dot).is_none() {
            let span = name.span;
            return Ok(leaf(ExprKind::Column { table: None, name }, span));
        }
        let column = self.expect_ident("column name", "after '.'")?;
        let span = name.span.until(column.span);
        let kind = ExprKind::Column {
            table: Some(name),
            name: column,
        };
        Ok(leaf(kind, span))
    }

    /// Function names are not checked here; `*` and `DISTINCT` are accepted for any name.
    fn function(&mut self, name: Ident) -> ParseResult<Parsed> {
        let open = self.bump();
        self.enter(Descent::NestedNode, open)?;
        let (args, args_height) = self.function_args()?;
        let close = self.expect(&TokenKind::RParen, "')'", "after function arguments")?;
        self.leave(Descent::NestedNode);
        let span = name.span.until(close);
        let at = name.span;
        self.node(ExprKind::Function { name, args }, span, args_height + 1, at)
    }

    fn function_args(&mut self) -> ParseResult<(FunctionArgs, usize)> {
        if self.at(&TokenKind::RParen) {
            let args = FunctionArgs::List {
                distinct: false,
                args: Vec::new(),
            };
            return Ok((args, 0));
        }
        if self.eat(&TokenKind::Star).is_some() {
            return Ok((FunctionArgs::Star, 0));
        }
        let distinct = self.eat_keyword(Keyword::Distinct).is_some();
        let (args, height) = self.expr_list_with_height()?;
        Ok((FunctionArgs::List { distinct, args }, height))
    }

    fn expr_list_with_height(&mut self) -> ParseResult<(Vec<Expr>, usize)> {
        let mut exprs = Vec::new();
        let mut height = 0;
        loop {
            let parsed = self.expr_bp(precedence::OR)?;
            height = height.max(parsed.height);
            exprs.push(*parsed.expr);
            if self.eat(&TokenKind::Comma).is_none() {
                return Ok((exprs, height));
            }
        }
    }

    /// Builds a node, rejecting trees taller than [`MAX_DEPTH`] at the operator `at`.
    fn node(&self, kind: ExprKind, span: Span, height: usize, at: Span) -> ParseResult<Parsed> {
        if height > MAX_DEPTH {
            return Err(self.depth_error(at));
        }
        Ok(Parsed {
            expr: Box::new(Spanned::new(kind, span)),
            height,
        })
    }
}

fn leaf(kind: ExprKind, span: Span) -> Parsed {
    Parsed {
        expr: Box::new(Spanned::new(kind, span)),
        height: 0,
    }
}

fn binary_op(kind: &TokenKind) -> Option<BinaryOp> {
    let op = match kind {
        TokenKind::Keyword(Keyword::Or) => BinaryOp::Or,
        TokenKind::Keyword(Keyword::And) => BinaryOp::And,
        TokenKind::Eq => BinaryOp::Eq,
        TokenKind::NotEq => BinaryOp::NotEq,
        TokenKind::Lt => BinaryOp::Lt,
        TokenKind::LtEq => BinaryOp::LtEq,
        TokenKind::Gt => BinaryOp::Gt,
        TokenKind::GtEq => BinaryOp::GtEq,
        TokenKind::Concat => BinaryOp::Concat,
        TokenKind::Plus => BinaryOp::Add,
        TokenKind::Minus => BinaryOp::Sub,
        TokenKind::Star => BinaryOp::Mul,
        TokenKind::Slash => BinaryOp::Div,
        TokenKind::Percent => BinaryOp::Mod,
        _ => return None,
    };
    Some(op)
}
