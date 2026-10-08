//! Typed expressions over the columns in scope, with a count of the
//! runtime errors they could raise (R-ERR).

use super::{Col, IntClass, Ty, int_literal, literal, real_literal, text_literal};
use crate::rng::Rng;

/// A column an expression may read: how to write it and its definition.
#[derive(Debug, Clone)]
pub(crate) struct InScope {
    pub text: String,
    pub col: Col,
}

pub(crate) struct ExprGen<'a> {
    pub rng: &'a mut Rng,
    pub scope: &'a [InScope],
    /// Runtime error sources generated so far (R-ERR).
    pub risks: u32,
    /// Whether a leaf since the last arithmetic operator could overflow.
    pub risky_operand: bool,
    /// Features used, for coverage.
    pub used: Vec<&'static str>,
}

impl ExprGen<'_> {
    fn column_of(&mut self, ty: Ty) -> Option<InScope> {
        let fits: Vec<&InScope> = self.scope.iter().filter(|c| c.col.ty == ty).collect();
        if fits.is_empty() {
            return None;
        }
        Some((*self.rng.pick(&fits)).clone())
    }

    fn leaf(&mut self, ty: Ty) -> String {
        if !self.rng.chance(3, 10)
            && let Some(column) = self.column_of(ty)
        {
            if column.col.class.risky() && ty == Ty::Int {
                self.risky_operand = true;
            }
            return column.text;
        }
        if self.rng.chance(1, 12) {
            return "NULL".to_string();
        }
        let class = if self.rng.chance(1, 8) {
            IntClass::Wild
        } else {
            IntClass::Small
        };
        if class == IntClass::Wild {
            self.risky_operand = true;
        }
        format!("({})", literal(self.rng, ty, class))
    }

    /// An expression of type `ty`, at most `depth` operators deep.
    pub fn of(&mut self, ty: Ty, depth: u32) -> String {
        if depth == 0 || self.rng.chance(2, 5) {
            return self.leaf(ty);
        }
        let d = depth - 1;
        match ty {
            Ty::Int => self.int(d),
            Ty::Real => self.real(d),
            Ty::Text => self.text(d),
            Ty::Bool => self.boolean(d),
        }
    }

    fn int(&mut self, d: u32) -> String {
        match self.rng.below(9) {
            0..=2 => {
                let op = *self.rng.pick(&["+", "-", "*"]);
                let (a, b) = (self.of(Ty::Int, d), self.of(Ty::Int, d));
                if op == "*" || self.take_risky() {
                    self.risks += 1;
                }
                format!("({a} {op} {b})")
            }
            3 => {
                let op = *self.rng.pick(&["/", "%"]);
                let a = self.of(Ty::Int, d);
                let divisor = if self.rng.chance(1, 2) {
                    format!(
                        "({})",
                        self.rng.range(1, 7) * if self.rng.chance(1, 2) { 1 } else { -1 }
                    )
                } else {
                    self.of(Ty::Int, d)
                };
                self.risks += 1;
                self.used.push("division");
                format!("({a} {op} {divisor})")
            }
            4 => {
                let a = self.of(Ty::Int, d);
                if self.take_risky() {
                    self.risks += 1;
                }
                if self.rng.chance(1, 2) {
                    format!("(- {a})")
                } else {
                    self.used.push("function");
                    format!("ABS({a})")
                }
            }
            5 => {
                self.used.push("function");
                format!("LENGTH({})", self.of(Ty::Text, d))
            }
            6 => self.case(Ty::Int, d),
            7 => self.coalesce(Ty::Int, d),
            _ => self.leaf(Ty::Int),
        }
    }

    fn real(&mut self, d: u32) -> String {
        match self.rng.below(7) {
            0..=1 => {
                let op = *self.rng.pick(&["+", "-", "*"]);
                let a = self.of(Ty::Real, d);
                let b = if self.rng.chance(1, 3) {
                    self.of(Ty::Int, d)
                } else {
                    self.of(Ty::Real, d)
                };
                let _ = self.take_risky();
                if op == "*" {
                    self.risks += 1;
                }
                format!("({a} {op} {b})")
            }
            2 => {
                let a = self.of(Ty::Real, d);
                self.risks += 1;
                self.used.push("division");
                format!("({a} / {})", self.of(Ty::Real, d))
            }
            3 => {
                self.used.push("function");
                format!("ABS({})", self.of(Ty::Real, d))
            }
            4 => self.case(Ty::Real, d),
            5 => {
                self.used.push("function");
                format!(
                    "COALESCE({}, {})",
                    self.of(Ty::Real, d),
                    self.of(Ty::Int, d)
                )
            }
            _ => format!("({})", real_literal(self.rng)),
        }
    }

    fn text(&mut self, d: u32) -> String {
        match self.rng.below(5) {
            0 => format!("({} || {})", self.of(Ty::Text, d), self.of(Ty::Text, d)),
            1 => {
                self.used.push("function");
                let f = *self.rng.pick(&["LOWER", "UPPER"]);
                format!("{f}({})", self.of(Ty::Text, d))
            }
            2 => self.case(Ty::Text, d),
            3 => self.coalesce(Ty::Text, d),
            _ => text_literal(self.rng),
        }
    }

    /// A BOOLEAN condition.
    pub fn boolean(&mut self, d: u32) -> String {
        match self.rng.below(11) {
            0..=2 => self.comparison(d),
            3 => {
                let op = *self.rng.pick(&["AND", "OR"]);
                format!("({} {op} {})", self.boolean(d), self.boolean(d))
            }
            4 => format!("(NOT {})", self.boolean(d)),
            5 => {
                let ty = self.any_type();
                let negated = if self.rng.chance(1, 2) { " NOT" } else { "" };
                format!("({} IS{negated} NULL)", self.of(ty, d))
            }
            6 => {
                let ty = self.ordered_type();
                let negated = if self.rng.chance(1, 4) { " NOT" } else { "" };
                format!(
                    "({}{negated} BETWEEN {} AND {})",
                    self.of(ty, d),
                    self.of(ty, d),
                    self.of(ty, d)
                )
            }
            7 => {
                let ty = self.ordered_type();
                let n = self.rng.range(1, 4);
                let items: Vec<String> = (0..n).map(|_| self.of(ty, d)).collect();
                let negated = if self.rng.chance(1, 4) { " NOT" } else { "" };
                format!("({}{negated} IN ({}))", self.of(ty, d), items.join(", "))
            }
            8 => {
                self.used.push("like");
                let pattern = *self.rng.pick(&[
                    "'%'", "'a%'", "'%b'", "'_'", "'%A%'", "'é_'", "'%\\_%'", "''",
                ]);
                let negated = if self.rng.chance(1, 4) { " NOT" } else { "" };
                format!("({}{negated} LIKE {pattern})", self.of(Ty::Text, d))
            }
            9 => self.case(Ty::Bool, d),
            _ => self.leaf(Ty::Bool),
        }
    }

    fn comparison(&mut self, d: u32) -> String {
        let ty = self.any_type();
        let op = *self.rng.pick(&["=", "<>", "<", "<=", ">", ">="]);
        let right_ty = if ty == Ty::Int && self.rng.chance(1, 4) {
            Ty::Real
        } else {
            ty
        };
        format!("({} {op} {})", self.of(ty, d), self.of(right_ty, d))
    }

    fn case(&mut self, ty: Ty, d: u32) -> String {
        self.used.push("case");
        let n = self.rng.range(1, 2);
        let mut out = "(CASE".to_string();
        for _ in 0..n {
            let when = self.boolean(d);
            let then = self.of(ty, d);
            out.push_str(&format!(" WHEN {when} THEN {then}"));
        }
        if self.rng.chance(2, 3) {
            out.push_str(&format!(" ELSE {}", self.of(ty, d)));
        }
        out.push_str(" END)");
        out
    }

    fn coalesce(&mut self, ty: Ty, d: u32) -> String {
        self.used.push("function");
        format!("COALESCE({}, {})", self.of(ty, d), self.of(ty, d))
    }

    fn any_type(&mut self) -> Ty {
        *self
            .rng
            .pick(&[Ty::Int, Ty::Int, Ty::Real, Ty::Text, Ty::Bool])
    }

    fn ordered_type(&mut self) -> Ty {
        *self.rng.pick(&[Ty::Int, Ty::Int, Ty::Real, Ty::Text])
    }

    fn take_risky(&mut self) -> bool {
        std::mem::take(&mut self.risky_operand)
    }
}

/// An INTEGER literal of a column's class, for conditions that an index
/// can serve.
pub(crate) fn key_literal(rng: &mut Rng, col: &Col) -> String {
    match col.ty {
        Ty::Int => format!("({})", int_literal(rng, col.class)),
        ty => literal(rng, ty, col.class),
    }
}
