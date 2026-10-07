CREATE TABLE s (dept TEXT, name TEXT, pay INTEGER, bonus REAL);
INSERT INTO s VALUES ('eng', 'a', 100, 1.5), ('eng', 'b', 200, NULL), ('ops', 'c', 50, 2.0), ('eng', 'd', 100, 0.5), ('ops', 'e', NULL, NULL);
SELECT dept, COUNT(*) AS n, COUNT(pay) AS np, COUNT(DISTINCT pay) AS dp, SUM(pay) AS total, AVG(pay) AS mean, MIN(name) AS lo, MAX(bonus) AS hi FROM s GROUP BY dept;
SELECT COUNT(*), SUM(bonus) FROM s;
SELECT pay, COUNT(*) AS n FROM s GROUP BY pay;
SELECT dept, SUM(pay) * 2 AS double FROM s GROUP BY dept;
