CREATE TABLE a (id INTEGER PRIMARY KEY, name TEXT);
CREATE TABLE b (aid INTEGER, tag TEXT);
INSERT INTO a VALUES (1, 'one'), (2, 'two'), (3, 'three');
INSERT INTO b VALUES (1, 'x'), (1, 'y'), (3, 'z'), (4, 'orphan');
SELECT a.name, b.tag FROM a JOIN b ON b.aid = a.id;
SELECT x.name, y.tag FROM b AS y INNER JOIN a AS x ON x.id = y.aid;
SELECT name, tag FROM a JOIN b ON a.id = b.aid WHERE tag <> 'x';
