//! Shrinking a failing case to the statements that matter, and the
//! reproducer script format.

/// One step of a generated case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// One SQL statement.
    Sql(String),
    /// Close the database and open it again (cairn only; the reference
    /// keeps its state).
    Reopen,
}

/// Removes chunks of `items`, halves at a time and then single items,
/// while `fails` still holds, keeping the order of what remains. Stops
/// after `max_runs` calls of `fails`.
pub fn minimize<T: Clone>(
    mut items: Vec<T>,
    mut fails: impl FnMut(&[T]) -> bool,
    max_runs: u32,
) -> Vec<T> {
    let mut runs = 0;
    let mut chunk = (items.len() / 2).max(1);
    loop {
        let mut removed = false;
        let mut start = 0;
        while start < items.len() && runs < max_runs {
            let end = (start + chunk).min(items.len());
            let candidate: Vec<T> = items
                .iter()
                .take(start)
                .chain(items.iter().skip(end))
                .cloned()
                .collect();
            runs += 1;
            if fails(&candidate) {
                items = candidate;
                removed = true;
            } else {
                start = end;
            }
        }
        if runs >= max_runs || (!removed && chunk == 1) || items.is_empty() {
            return items;
        }
        if !removed {
            chunk = (chunk / 2).max(1);
        }
        chunk = chunk.min(items.len().max(1));
    }
}

/// A script with one statement per line and `-- @reopen` for reopens. Without
/// reopens it runs as is: `cairn --create x.db < repro.sql`.
pub fn to_script(steps: &[Step]) -> String {
    let mut out = String::new();
    for step in steps {
        match step {
            Step::Sql(sql) => {
                out.push_str(sql);
                out.push_str(";\n");
            }
            Step::Reopen => out.push_str("-- @reopen\n"),
        }
    }
    out
}

/// Reads a script written by [`to_script`].
pub fn from_script(text: &str) -> Vec<Step> {
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| match line {
            "-- @reopen" => Step::Reopen,
            sql => Step::Sql(sql.strip_suffix(';').unwrap_or(sql).to_string()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shrinks_to_exactly_the_needed_steps_in_order() {
        let steps: Vec<u32> = (0..50).collect();
        let fails = |s: &[u32]| s.contains(&7) && s.contains(&31);
        assert_eq!(minimize(steps, fails, 2000), vec![7, 31]);
    }

    #[test]
    fn respects_the_run_limit() {
        let mut calls = 0;
        let steps: Vec<u32> = (0..64).collect();
        let _ = minimize(
            steps,
            |s| {
                calls += 1;
                s.contains(&3)
            },
            10,
        );
        assert_eq!(calls, 10);
    }

    #[test]
    fn scripts_round_trip() {
        let steps = vec![
            Step::Sql("CREATE TABLE t (a INTEGER)".into()),
            Step::Reopen,
            Step::Sql("SELECT 'x;y' FROM t".into()),
        ];
        let script = to_script(&steps);
        assert_eq!(
            script,
            "CREATE TABLE t (a INTEGER);\n-- @reopen\nSELECT 'x;y' FROM t;\n"
        );
        assert_eq!(from_script(&script), steps);
    }
}
