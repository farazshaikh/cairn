//! Differential cases: a schema, data and a statement sequence.

use super::expr::{ExprGen, InScope, key_literal};
use super::{CaseStep, Col, Coverage, IntClass, Tab, Ty, literal};
use crate::minimize::Step;
use crate::rng::Rng;

/// One generated case and the features it used.
pub fn case(rng: &mut Rng) -> (Vec<CaseStep>, Coverage) {
    let mut g = Gen {
        rng,
        tables: Vec::new(),
        steps: Vec::new(),
        coverage: Coverage::default(),
        in_txn: false,
        next_index: 0,
    };
    g.coverage.add("R-SIZE");
    let table_count = g.rng.range(1, 3);
    for t in 0..table_count {
        let tab = g.table(t);
        g.push(create_table(&tab));
        if tab.constrained() {
            g.coverage.add("constraint_table");
        }
        g.tables.push(tab);
    }
    let late_index = g.rng.chance(1, 2);
    if !late_index {
        g.create_index();
    }
    for t in 0..g.tables.len() {
        for _ in 0..g.rng.range(1, 4) {
            g.insert(t);
        }
    }
    if late_index {
        g.create_index();
    }
    for _ in 0..g.rng.range(5, 25) {
        g.statement();
    }
    let mut coverage = g.coverage;
    coverage.counts.sort_unstable();
    coverage.counts.dedup_by(|a, b| a.0 == b.0);
    for c in &mut coverage.counts {
        c.1 = 1;
    }
    (g.steps, coverage)
}

struct Gen<'a> {
    rng: &'a mut Rng,
    tables: Vec<Tab>,
    steps: Vec<CaseStep>,
    coverage: Coverage,
    in_txn: bool,
    next_index: u32,
}

fn create_table(tab: &Tab) -> String {
    let columns: Vec<String> = tab
        .cols
        .iter()
        .map(|c| {
            let mut def = format!("{} {}", c.name, c.ty.sql());
            if c.pk {
                def.push_str(" PRIMARY KEY");
            }
            if c.not_null {
                def.push_str(" NOT NULL");
            }
            if c.unique {
                def.push_str(" UNIQUE");
            }
            def
        })
        .collect();
    format!("CREATE TABLE {} ({})", tab.name, columns.join(", "))
}

impl Gen<'_> {
    fn push(&mut self, sql: String) {
        self.steps.push(CaseStep::sql(sql));
    }

    fn table(&mut self, t: i64) -> Tab {
        let count = self.rng.range(1, 6);
        let pk = self.rng.below(10);
        let mut cols = Vec::new();
        for c in 0..count {
            let ty = if c == 0 && pk < 3 {
                Ty::Int
            } else if c == 0 && pk == 3 {
                Ty::Text
            } else {
                *self
                    .rng
                    .pick(&[Ty::Int, Ty::Int, Ty::Real, Ty::Text, Ty::Bool])
            };
            let class = match (c == 0 && pk < 3, self.rng.below(20)) {
                (true, _) | (false, 0..=11) => IntClass::Small,
                (false, 12..=14) => IntClass::Positive,
                (false, 15..=16) => IntClass::Negative,
                _ => IntClass::Wild,
            };
            if ty == Ty::Real {
                self.coverage.add("R-REAL");
            }
            cols.push(Col {
                name: format!("c{c}"),
                ty,
                class,
                pk: c == 0 && pk <= 3,
                not_null: self.rng.chance(1, 7),
                unique: c > 0 && self.rng.chance(1, 7),
            });
        }
        let indexed = cols
            .iter()
            .enumerate()
            .filter(|(_, c)| c.pk || c.unique)
            .map(|(i, _)| i)
            .collect();
        Tab {
            name: format!("t{t}"),
            cols,
            indexed,
        }
    }

    fn create_index(&mut self) {
        let Some(t) = self.some_table() else {
            return;
        };
        let tab = &self.tables[t];
        let column = self.rng.index(tab.cols.len());
        let unique = self.rng.chance(3, 10);
        let sql = format!(
            "CREATE {}INDEX i{} ON {} ({})",
            if unique { "UNIQUE " } else { "" },
            self.next_index,
            tab.name,
            tab.cols[column].name
        );
        self.next_index += 1;
        self.tables[t].indexed.push(column);
        self.coverage.add("create_index");
        self.push(sql);
    }

    fn some_table(&mut self) -> Option<usize> {
        (!self.tables.is_empty()).then(|| self.rng.index(self.tables.len()))
    }

    fn value(&mut self, col: &Col) -> String {
        if self.rng.chance(1, 10) {
            return "NULL".to_string();
        }
        if self.rng.chance(1, 40) {
            self.coverage.add("type_error");
            return match col.ty {
                Ty::Text => "1".to_string(),
                _ => "'x'".to_string(),
            };
        }
        if col.pk && col.ty == Ty::Int {
            return format!("{}", self.rng.range(-20, 20));
        }
        literal(self.rng, col.ty, col.class)
    }

    fn insert(&mut self, t: usize) {
        let tab = self.tables[t].clone();
        let mut targets: Vec<usize> = (0..tab.cols.len()).collect();
        let named = self.rng.chance(1, 4);
        if named {
            for i in (1..targets.len()).rev() {
                let j = self.rng.index(i + 1);
                targets.swap(i, j);
            }
            let keep = self.rng.range(1, targets.len() as i64) as usize;
            targets.truncate(keep);
        }
        let rows: Vec<String> = (0..self.rng.range(1, 8))
            .map(|_| {
                let values: Vec<String> =
                    targets.iter().map(|&c| self.value(&tab.cols[c])).collect();
                format!("({})", values.join(", "))
            })
            .collect();
        let columns = if named {
            let names: Vec<&str> = targets.iter().map(|&c| tab.cols[c].name.as_str()).collect();
            format!(" ({})", names.join(", "))
        } else {
            String::new()
        };
        self.coverage.add("insert");
        self.push(format!(
            "INSERT INTO {}{columns} VALUES {}",
            tab.name,
            rows.join(", ")
        ));
    }

    fn statement(&mut self) {
        match self.rng.below(100) {
            0..=49 => self.select(),
            50..=63 => {
                if let Some(t) = self.some_table() {
                    self.insert(t);
                }
            }
            64..=73 => self.update(),
            74..=80 => self.delete(),
            81..=91 => self.transaction(),
            92..=95 => {
                if !self.in_txn {
                    self.coverage.add("reopen");
                    self.steps.push(CaseStep {
                        step: Step::Reopen,
                        ..CaseStep::sql(String::new())
                    });
                }
            }
            96..=97 => self.create_index(),
            _ => self.drop_table(),
        }
    }

    fn transaction(&mut self) {
        let misplaced = self.rng.chance(1, 20);
        let sql = match (self.in_txn, misplaced) {
            (false, false) => {
                self.in_txn = true;
                self.coverage.add("begin");
                "BEGIN"
            }
            (true, false) => {
                self.in_txn = false;
                if self.rng.chance(1, 2) {
                    self.coverage.add("commit");
                    "COMMIT"
                } else {
                    self.coverage.add("rollback");
                    "ROLLBACK"
                }
            }
            (false, true) => {
                self.coverage.add("txn_misuse");
                *self.rng.pick(&["COMMIT", "ROLLBACK"])
            }
            (true, true) => {
                self.coverage.add("txn_misuse");
                "BEGIN"
            }
        };
        self.push(sql.to_string());
    }

    fn drop_table(&mut self) {
        if self.tables.len() > 1 && self.rng.chance(1, 2) {
            let t = self.rng.index(self.tables.len());
            let tab = self.tables.remove(t);
            self.push(format!("DROP TABLE {}", tab.name));
        } else {
            self.push("DROP TABLE IF EXISTS nothere".to_string());
        }
        self.coverage.add("drop_table");
    }

    fn table_scope(tab: &Tab, alias: Option<&str>) -> Vec<InScope> {
        tab.cols
            .iter()
            .map(|c| InScope {
                text: match alias {
                    Some(a) => format!("{a}.{}", c.name),
                    None => c.name.clone(),
                },
                col: c.clone(),
            })
            .collect()
    }

    fn finish(&mut self, mut step: CaseStep, risks: u32, used: Vec<&'static str>) {
        for feature in used {
            self.coverage.add(feature);
        }
        step.multi_error = risks >= 2;
        if step.multi_error {
            self.coverage.add("R-ERR");
        }
        self.steps.push(step);
    }

    fn update(&mut self) {
        let Some(t) = self.some_table() else {
            return;
        };
        let tab = self.tables[t].clone();
        let scope = Self::table_scope(&tab, None);
        let mut eg = ExprGen {
            rng: self.rng,
            scope: &scope,
            risks: u32::from(tab.constrained()),
            risky_operand: false,
            used: vec!["update"],
        };
        let first = eg.rng.index(tab.cols.len());
        let mut columns = vec![first];
        if tab.cols.len() > 1 && eg.rng.chance(1, 3) {
            let second = (first + 1) % tab.cols.len();
            columns.push(second);
        }
        let sets: Vec<String> = columns
            .iter()
            .map(|&c| {
                let depth = eg.rng.below(3) as u32;
                format!("{} = {}", tab.cols[c].name, eg.of(tab.cols[c].ty, depth))
            })
            .collect();
        let mut sql = format!("UPDATE {} SET {}", tab.name, sets.join(", "));
        if eg.rng.chance(7, 10) {
            eg.used.push("where");
            sql.push_str(&format!(" WHERE {}", eg.boolean(2)));
        }
        let (risks, used) = (eg.risks, eg.used);
        self.finish(CaseStep::sql(sql), risks, used);
    }

    fn delete(&mut self) {
        let Some(t) = self.some_table() else {
            return;
        };
        let tab = self.tables[t].clone();
        let scope = Self::table_scope(&tab, None);
        let mut eg = ExprGen {
            rng: self.rng,
            scope: &scope,
            risks: 0,
            risky_operand: false,
            used: vec!["delete"],
        };
        let mut sql = format!("DELETE FROM {}", tab.name);
        if eg.rng.chance(17, 20) {
            eg.used.push("where");
            sql.push_str(&format!(" WHERE {}", eg.boolean(2)));
        }
        let (risks, used) = (eg.risks, eg.used);
        self.finish(CaseStep::sql(sql), risks, used);
    }

    fn select(&mut self) {
        if self.tables.is_empty() {
            return;
        }
        let joins = match self.rng.below(10) {
            0..=5 => 0,
            6..=8 => 1,
            _ => 2,
        };
        let mut used = vec!["select"];
        let mut scope: Vec<InScope> = Vec::new();
        let mut from = String::new();
        let mut on_risks = 0;
        let first = self.rng.index(self.tables.len());
        let first_tab = self.tables[first].clone();
        if joins == 0 {
            from.push_str(&first_tab.name);
            scope.extend(Self::table_scope(&first_tab, None));
        } else {
            from.push_str(&format!("{} AS a0", first_tab.name));
            scope.extend(Self::table_scope(&first_tab, Some("a0")));
            for j in 1..=joins {
                let t = self.rng.index(self.tables.len());
                let tab = self.tables[t].clone();
                let alias = format!("a{j}");
                let inner = Self::table_scope(&tab, Some(&alias));
                let left = self.rng.chance(1, 3);
                used.push(if left { "left_join" } else { "inner_join" });
                let pairs: Vec<(String, String)> = inner
                    .iter()
                    .flat_map(|i| {
                        scope
                            .iter()
                            .filter(move |o| o.col.ty == i.col.ty)
                            .map(move |o| (i.text.clone(), o.text.clone()))
                    })
                    .collect();
                let mut on = if pairs.is_empty() {
                    "TRUE".to_string()
                } else {
                    let (a, b) = self.rng.pick(&pairs).clone();
                    format!("{a} = {b}")
                };
                scope.extend(inner);
                if self.rng.chance(1, 4) {
                    let mut eg = ExprGen {
                        rng: self.rng,
                        scope: &scope,
                        risks: 0,
                        risky_operand: false,
                        used: Vec::new(),
                    };
                    on = format!("{on} AND {}", eg.boolean(1));
                    on_risks += eg.risks;
                    used.extend(eg.used);
                }
                from.push_str(&format!(
                    " {} {} AS {alias} ON {on}",
                    if left { "LEFT JOIN" } else { "JOIN" },
                    tab.name
                ));
            }
        }
        let mut eg = ExprGen {
            rng: self.rng,
            scope: &scope,
            risks: on_risks,
            risky_operand: false,
            used,
        };
        let grouped = eg.rng.chance(3, 10);
        let mut items: Vec<String> = Vec::new();
        let mut having = None;
        let mut group_by = Vec::new();
        if grouped {
            eg.used.push("aggregate");
            let keys = eg.rng.below(3) as usize;
            for _ in 0..keys {
                let column = eg.rng.pick(&scope).text.clone();
                if !group_by.contains(&column) {
                    group_by.push(column);
                }
            }
            if !group_by.is_empty() {
                eg.used.push("group_by");
            }
            items.extend(group_by.iter().cloned());
            for _ in 0..eg.rng.range(1, 3) {
                let call = aggregate(&mut eg, &scope);
                items.push(call);
            }
            if eg.rng.chance(3, 10) {
                eg.used.push("having");
                let call = aggregate(&mut eg, &scope);
                having = Some(match eg.rng.below(3) {
                    0 => format!("COUNT(*) > {}", eg.rng.range(0, 3)),
                    1 => format!("{call} IS NOT NULL"),
                    _ => format!("({call} IS NULL) OR COUNT(*) >= 1"),
                });
            }
        } else if eg.rng.chance(1, 10) {
            items.push("*".to_string());
        } else {
            for i in 0..eg.rng.range(1, 4) {
                let ty = *eg.rng.pick(&[Ty::Int, Ty::Real, Ty::Text, Ty::Bool]);
                let depth = eg.rng.below(3) as u32;
                let expr = eg.of(ty, depth);
                if eg.rng.chance(3, 10) {
                    items.push(format!("{expr} AS x{i}"));
                } else {
                    items.push(expr);
                }
            }
        }
        let width = if items == ["*"] {
            scope.len()
        } else {
            items.len()
        };
        let mut sql = "SELECT ".to_string();
        let distinct = !grouped && eg.rng.chance(15, 100);
        if distinct {
            eg.used.push("distinct");
            sql.push_str("DISTINCT ");
        }
        sql.push_str(&items.join(", "));
        sql.push_str(&format!(" FROM {from}"));
        let mut indexable = false;
        if eg.rng.chance(7, 10) {
            eg.used.push("where");
            let depth = eg.rng.below(3) as u32;
            let mut cond = eg.boolean(depth);
            if joins == 0 && !first_tab.indexed.is_empty() && eg.rng.chance(1, 2) {
                let column = *eg.rng.pick(&first_tab.indexed);
                let col = &first_tab.cols[column];
                let op = *eg.rng.pick(&["=", "=", "<", "<=", ">", ">="]);
                let value = key_literal(eg.rng, col);
                cond = format!("{} {op} {value} AND {cond}", col.name);
                indexable = true;
            }
            sql.push_str(&format!(" WHERE {cond}"));
        }
        if !group_by.is_empty() {
            sql.push_str(&format!(" GROUP BY {}", group_by.join(", ")));
        }
        if let Some(h) = having {
            sql.push_str(&format!(" HAVING {h}"));
        }
        let mut order_keys = None;
        if eg.rng.chance(1, 2) {
            eg.used.push("order_by");
            let mut positions: Vec<usize> = (0..width).collect();
            for i in (1..positions.len()).rev() {
                let j = eg.rng.index(i + 1);
                positions.swap(i, j);
            }
            let all = eg.rng.chance(1, 2);
            if !all {
                positions.truncate(eg.rng.range(1, width as i64) as usize);
            }
            let keys: Vec<String> = positions
                .iter()
                .map(|p| {
                    let desc = if eg.rng.chance(1, 3) { " DESC" } else { "" };
                    format!("{}{desc}", p + 1)
                })
                .collect();
            sql.push_str(&format!(" ORDER BY {}", keys.join(", ")));
            if positions.len() == width && eg.rng.chance(2, 5) {
                eg.used.push("limit");
                eg.used.push("R-LIMIT");
                sql.push_str(&format!(" LIMIT {}", eg.rng.range(0, 5)));
                if eg.rng.chance(1, 2) {
                    sql.push_str(&format!(" OFFSET {}", eg.rng.range(0, 3)));
                }
            }
            order_keys = Some(positions);
        }
        let (risks, used) = (eg.risks, eg.used);
        let step = CaseStep {
            step: Step::Sql(sql),
            order_keys,
            multi_error: false,
            indexable,
        };
        self.finish(step, risks, used);
    }
}

/// An aggregate call over one column, following R-SUM.
fn aggregate(eg: &mut ExprGen, scope: &[InScope]) -> String {
    let column = eg.rng.pick(scope).clone();
    match eg.rng.below(7) {
        0 => {
            eg.used.push("count");
            "COUNT(*)".to_string()
        }
        1 => {
            eg.used.push("count");
            let distinct = if eg.rng.chance(1, 3) { "DISTINCT " } else { "" };
            format!("COUNT({distinct}{})", column.text)
        }
        2 | 3 if matches!(column.col.ty, Ty::Int | Ty::Real) => {
            let summable = column.col.ty == Ty::Real || column.col.class.summable();
            if eg.rng.chance(1, 2) && summable {
                eg.used.push("sum");
                eg.used.push("R-SUM");
                if column.col.class.risky() && column.col.ty == Ty::Int {
                    eg.risks += 1;
                }
                format!("SUM({})", column.text)
            } else {
                eg.used.push("avg");
                format!("AVG({})", column.text)
            }
        }
        _ => {
            eg.used.push("min_max");
            let f = *eg.rng.pick(&["MIN", "MAX"]);
            format!("{f}({})", column.text)
        }
    }
}
