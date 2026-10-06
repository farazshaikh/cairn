//! Malformed inputs (milestone AC2, AC7, AC8): each case asserts the exact
//! message, byte offset, line and column. The table is the normative error
//! catalogue in the M2 LLD, section 7.

use cairn_sql::parse;

struct Case {
    input: String,
    message: &'static str,
    start: usize,
    line: usize,
    column: usize,
}

fn case(
    input: impl Into<String>,
    message: &'static str,
    start: usize,
    line: usize,
    column: usize,
) -> Case {
    Case {
        input: input.into(),
        message,
        start,
        line,
        column,
    }
}

fn cases() -> Vec<Case> {
    vec![
        // Lexer (L1-L15).
        case("SELECT 'abc", "unterminated string literal", 7, 1, 8),
        case("SELECT \"abc", "unterminated quoted identifier", 7, 1, 8),
        case("SELECT 1 /* x", "unterminated block comment", 9, 1, 10),
        case("SELECT @", "unexpected character '@'", 7, 1, 8),
        case("SELECT #", "unexpected character '#'", 7, 1, 8),
        case("SELECT 1 ! 2", "unexpected character '!'", 9, 1, 10),
        case("SELECT a | b", "unexpected character '|'", 9, 1, 10),
        case(
            "SELECT 9223372036854775808",
            "integer literal out of range",
            7,
            1,
            8,
        ),
        case("SELECT 1e999", "real literal out of range", 7, 1, 8),
        case(
            "SELECT 1e",
            "expected digits after exponent in number literal",
            7,
            1,
            8,
        ),
        case(
            "SELECT 12abc",
            "unexpected character 'a' after number",
            9,
            1,
            10,
        ),
        case("SELECT \"\"", "empty quoted identifier", 7, 1, 8),
        case("SELECT 1,\n  'abc", "unterminated string literal", 12, 2, 3),
        case("SELECT 'éé' @", "unexpected character '@'", 14, 1, 13),
        case("SELECT 1 -- ok\n@", "unexpected character '@'", 15, 2, 1),
        // Parser (P1-P49).
        case(
            "CREATE TABLE t (a INTEGER FROM",
            "expected ')' after column list, found 'FROM'",
            26,
            1,
            27,
        ),
        case(
            "CREATE TABLE t (a VARCHAR)",
            "expected column type INTEGER, REAL, TEXT or BOOLEAN, found 'VARCHAR'",
            18,
            1,
            19,
        ),
        case(
            "CREATE TABLE (a INTEGER)",
            "expected table name after TABLE, found '('",
            13,
            1,
            14,
        ),
        case(
            "CREATE TABLE t ()",
            "expected column name, found ')'",
            16,
            1,
            17,
        ),
        case(
            "CREATE TABLE t (a INTEGER PRIMARY)",
            "expected KEY after PRIMARY, found ')'",
            33,
            1,
            34,
        ),
        case(
            "CREATE TABLE t (a INTEGER NOT NULL NOT NULL)",
            "duplicate constraint NOT NULL",
            35,
            1,
            36,
        ),
        case(
            "CREATE VIEW v",
            "expected TABLE, INDEX or UNIQUE after CREATE, found 'VIEW'",
            7,
            1,
            8,
        ),
        case(
            "CREATE INDEX i ON t",
            "expected '(' after table name, found end of input",
            19,
            1,
            20,
        ),
        case(
            "CREATE INDEX i ON t (a, b)",
            "expected ')' after index column, found ','",
            22,
            1,
            23,
        ),
        case(
            "DROP TABLE IF t",
            "expected EXISTS after IF, found 't'",
            14,
            1,
            15,
        ),
        case("DROP t", "expected TABLE after DROP, found 't'", 5, 1, 6),
        case(
            "INSERT INTO t VALUES",
            "expected '(' after VALUES, found end of input",
            20,
            1,
            21,
        ),
        case(
            "INSERT INTO t VALUES ()",
            "expected expression, found ')'",
            22,
            1,
            23,
        ),
        case(
            "INSERT INTO t (a b) VALUES (1)",
            "expected ')' after column list, found 'b'",
            17,
            1,
            18,
        ),
        case(
            "INSERT t VALUES (1)",
            "expected INTO after INSERT, found 't'",
            7,
            1,
            8,
        ),
        case(
            "UPDATE t SET",
            "expected column name after SET, found end of input",
            12,
            1,
            13,
        ),
        case(
            "UPDATE t SET a 1",
            "expected '=' after column name, found '1'",
            15,
            1,
            16,
        ),
        case("DELETE t", "expected FROM after DELETE, found 't'", 7, 1, 8),
        case(
            "DELETE FROM t WHERE",
            "expected expression, found end of input",
            19,
            1,
            20,
        ),
        case(
            "SELECT FROM t",
            "expected expression, found 'FROM'",
            7,
            1,
            8,
        ),
        case(
            "SELECT a FROM",
            "expected table name after FROM, found end of input",
            13,
            1,
            14,
        ),
        case(
            "SELECT * FROM t JOIN u",
            "expected ON after join table, found end of input",
            22,
            1,
            23,
        ),
        case(
            "SELECT a FROM t ORDER a",
            "expected BY after ORDER, found 'a'",
            22,
            1,
            23,
        ),
        case(
            "SELECT a FROM t GROUP BY",
            "expected expression, found end of input",
            24,
            1,
            25,
        ),
        case(
            "SELECT a FROM t LIMIT 'x'",
            "expected integer after LIMIT, found ''x''",
            22,
            1,
            23,
        ),
        case(
            "SELECT a FROM t ORDER BY a WHERE b",
            "expected ';' or end of input after statement, found 'WHERE'",
            27,
            1,
            28,
        ),
        case(
            "SELECT 1 SELECT 2",
            "expected ';' or end of input after statement, found 'SELECT'",
            9,
            1,
            10,
        ),
        case("", "expected statement, found end of input", 0, 1, 1),
        case("SELECT 1;;", "expected statement, found ';'", 9, 1, 10),
        case(
            "SELECT (1 + 2",
            "expected ')' after expression, found end of input",
            13,
            1,
            14,
        ),
        case(
            "SELECT 1 +",
            "expected expression, found end of input",
            10,
            1,
            11,
        ),
        case(
            "SELECT a NOT b",
            "expected BETWEEN, IN or LIKE after NOT, found 'b'",
            13,
            1,
            14,
        ),
        case(
            "SELECT a BETWEEN 1 OR 2",
            "expected AND after BETWEEN lower bound, found 'OR'",
            19,
            1,
            20,
        ),
        case(
            "SELECT a IN ()",
            "expected expression, found ')'",
            13,
            1,
            14,
        ),
        case(
            "SELECT a IS 1",
            "expected NULL or NOT after IS, found '1'",
            12,
            1,
            13,
        ),
        case(
            "SELECT CASE a END",
            "expected WHEN after CASE, found 'a'",
            12,
            1,
            13,
        ),
        case(
            "SELECT CASE WHEN a THEN 1",
            "expected WHEN, ELSE or END in CASE, found end of input",
            25,
            1,
            26,
        ),
        case(
            "SELECT count(a",
            "expected ')' after function arguments, found end of input",
            14,
            1,
            15,
        ),
        case(
            "SELECT a AS FROM t",
            "expected alias after AS, found 'FROM'",
            12,
            1,
            13,
        ),
        case(
            "SELECT t. FROM t",
            "expected column name after '.', found 'FROM'",
            10,
            1,
            11,
        ),
        case(
            format!("SELECT {}1{}", "(".repeat(201), ")".repeat(201)),
            "expression nesting exceeds the limit of 200",
            207,
            1,
            208,
        ),
        case(
            "SELECT a\nFROM t\nWHERE",
            "expected expression, found end of input",
            21,
            3,
            6,
        ),
        case(
            format!("SELECT 1{}", " + 1".repeat(201)),
            "expression nesting exceeds the limit of 200",
            809,
            1,
            810,
        ),
        case(
            format!("SELECT {}a", "NOT ".repeat(201)),
            "expression nesting exceeds the limit of 200",
            807,
            1,
            808,
        ),
        case(
            "SELECT a FROM t LEFT u",
            "expected JOIN after LEFT, found 'u'",
            21,
            1,
            22,
        ),
        case(
            "SELECT a FROM t AS",
            "expected alias after AS, found end of input",
            18,
            1,
            19,
        ),
        case("FOO", "expected statement, found 'FOO'", 0, 1, 1),
        case(
            "SELECT a = NOT b",
            "expected expression, found 'NOT'",
            11,
            1,
            12,
        ),
        case(
            "CREATE UNIQUE TABLE t (a INTEGER)",
            "expected INDEX after UNIQUE, found 'TABLE'",
            14,
            1,
            15,
        ),
    ]
}

#[test]
fn every_malformed_input_reports_its_exact_message_and_span() {
    let cases = cases();
    assert!(cases.len() >= 30, "only {} malformed inputs", cases.len());
    assert_eq!(cases.len(), 64);
    for case in &cases {
        let error = match parse(&case.input) {
            Ok(statements) => panic!("{:?} parsed: {statements:?}", case.input),
            Err(error) => error,
        };
        let observed = (
            error.message.as_str(),
            error.span.start,
            error.span.line,
            error.span.column,
        );
        let expected = (case.message, case.start, case.line, case.column);
        assert_eq!(observed, expected, "input {:?}", case.input);
        assert_eq!(
            error.to_string().lines().count(),
            3,
            "input {:?}",
            case.input
        );
    }
}

#[test]
fn display_shows_the_offending_line_with_a_caret() {
    let error = parse("SELECT 1,\n  'abc").expect_err("unterminated string");
    assert_eq!(
        error.to_string(),
        "line 2, column 3: unterminated string literal\n  'abc\n  ^"
    );
    let error = parse("SELECT a\nFROM t\nWHERE").expect_err("missing expression");
    assert_eq!(
        error.to_string(),
        "line 3, column 6: expected expression, found end of input\nWHERE\n     ^"
    );
}

#[test]
fn long_and_multi_line_tokens_are_shortened_after_found() {
    let long = format!("SELECT 1 '{}'", "x".repeat(60));
    let error = parse(&long).expect_err("missing separator");
    assert_eq!(
        error.message,
        format!(
            "expected ';' or end of input after statement, found ''{}...'",
            "x".repeat(39)
        )
    );
    let error = parse("SELECT 1 'a\nb'").expect_err("missing separator");
    assert_eq!(
        error.message,
        "expected ';' or end of input after statement, found ''a...'"
    );
}

#[test]
fn errors_name_the_context_after_if_not_exists_and_in_later_list_items() {
    let cases = [
        (
            "CREATE TABLE IF EXISTS t (a INTEGER)",
            "expected NOT after IF, found 'EXISTS'",
        ),
        (
            "CREATE TABLE IF NOT t (a INTEGER)",
            "expected EXISTS after IF NOT, found 't'",
        ),
        (
            "CREATE TABLE IF NOT EXISTS (a INTEGER)",
            "expected table name after EXISTS, found '('",
        ),
        (
            "DROP TABLE IF EXISTS",
            "expected table name after EXISTS, found end of input",
        ),
        (
            "INSERT INTO t VALUES (1), 2",
            "expected '(' after ',', found '2'",
        ),
        (
            "INSERT INTO t VALUES (1 2)",
            "expected ')' after row values, found '2'",
        ),
        (
            "INSERT INTO t (a) (1)",
            "expected VALUES after column list, found '('",
        ),
        (
            "UPDATE t SET a = 1, = 2",
            "expected column name after ',', found '='",
        ),
        (
            "SELECT a FROM t INNER u",
            "expected JOIN after INNER, found 'u'",
        ),
        (
            "SELECT a FROM t LIMIT 1 OFFSET x",
            "expected integer after OFFSET, found 'x'",
        ),
        ("SELECT a IS NOT 1", "expected NULL after IS NOT, found '1'"),
        (
            "SELECT CASE WHEN a THEN 1 ELSE 2",
            "expected END after ELSE result, found end of input",
        ),
        (
            "SELECT CASE WHEN a 1 END",
            "expected THEN after WHEN condition, found '1'",
        ),
        ("SELECT a IN 1", "expected '(' after IN, found '1'"),
        (
            "SELECT f(a, b",
            "expected ')' after function arguments, found end of input",
        ),
        (
            "SELECT a IS NULL || b",
            "expected ';' or end of input after statement, found '||'",
        ),
        ("SELECT -NOT a", "expected expression, found 'NOT'"),
        (
            "CREATE INDEX i t (a)",
            "expected ON after index name, found 't'",
        ),
        (
            "CREATE TABLE t (a INTEGER UNIQUE UNIQUE)",
            "duplicate constraint UNIQUE",
        ),
        (
            "CREATE TABLE t (a INTEGER PRIMARY KEY PRIMARY KEY)",
            "duplicate constraint PRIMARY KEY",
        ),
    ];
    for (input, message) in cases {
        let error = parse(input).expect_err(input);
        assert_eq!(error.message, message, "input {input:?}");
    }
}
