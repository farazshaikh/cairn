CREATE TABLE t (a INTEGER, b TEXT, c REAL);
INSERT INTO t VALUES (1, 'one', 1.5);
INSERT INTO t (b) VALUES ('only b');
INSERT INTO t (c, a) VALUES (2.5, 2), (3.5, 3);
SELECT a, b, c FROM t;
