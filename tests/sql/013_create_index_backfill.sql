CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT);
INSERT INTO t VALUES (1, 30, 'x'), (2, 10, 'y'), (3, 20, 'z'), (4, NULL, 'w'), (5, 10, 'v');
CREATE INDEX t_a ON t (a);
SELECT id, a FROM t WHERE a = 10;
SELECT id, a FROM t WHERE a >= 20;
EXPLAIN SELECT id, a FROM t WHERE a >= 20;
INSERT INTO t VALUES (6, 15, 'u');
SELECT id FROM t WHERE a < 20;
