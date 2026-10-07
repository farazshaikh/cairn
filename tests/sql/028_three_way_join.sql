CREATE TABLE c (id INTEGER PRIMARY KEY, name TEXT);
CREATE TABLE o (id INTEGER PRIMARY KEY, cid INTEGER);
CREATE TABLE l (oid INTEGER, item TEXT);
INSERT INTO c VALUES (1, 'ann'), (2, 'bob'), (3, 'cy');
INSERT INTO o VALUES (10, 1), (11, 1), (12, 2);
INSERT INTO l VALUES (10, 'pen'), (10, 'ink'), (12, 'cup');
SELECT c.name, o.id, l.item FROM c JOIN o ON o.cid = c.id LEFT JOIN l ON l.oid = o.id;
SELECT c.name, COUNT(l.item) AS items FROM c LEFT JOIN o ON o.cid = c.id LEFT JOIN l ON l.oid = o.id GROUP BY c.name;
