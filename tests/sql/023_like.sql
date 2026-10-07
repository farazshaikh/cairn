CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT);
INSERT INTO t VALUES (1, 'apple'), (2, 'Apple'), (3, 'apricot'), (4, 'a'), (5, ''), (6, NULL), (7, 'naïve');
SELECT id FROM t WHERE s LIKE 'ap%';
SELECT id FROM t WHERE s LIKE '_pple';
SELECT id FROM t WHERE s LIKE '%';
SELECT id FROM t WHERE s NOT LIKE '%p%';
SELECT id FROM t WHERE s LIKE 'na_ve';
SELECT 'abc' LIKE 'a%c' AS a, 'abc' LIKE 'A%' AS b, NULL LIKE 'a' AS c, 'ab' LIKE 'a' AS d;
SELECT id FROM t WHERE id LIKE '1';
