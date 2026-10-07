CREATE TABLE t (g TEXT, v INTEGER);
SELECT COUNT(*) AS c, COUNT(v) AS cv, SUM(v) AS s, AVG(v) AS a, MIN(v) AS lo, MAX(v) AS hi FROM t;
SELECT g, COUNT(*) AS c FROM t GROUP BY g;
INSERT INTO t VALUES ('x', NULL), ('x', NULL), (NULL, 3);
SELECT g, COUNT(*) AS c, COUNT(v) AS cv, SUM(v) AS s, AVG(v) AS a, MIN(v) AS lo FROM t GROUP BY g;
