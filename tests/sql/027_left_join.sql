CREATE TABLE a (id INTEGER PRIMARY KEY, name TEXT);
CREATE TABLE b (aid INTEGER, tag TEXT);
INSERT INTO a VALUES (1, 'one'), (2, 'two'), (3, NULL);
INSERT INTO b VALUES (1, 'x'), (NULL, 'n');
SELECT a.id, b.tag FROM a LEFT JOIN b ON b.aid = a.id;
SELECT a.id, b.tag FROM a LEFT JOIN b ON b.aid = a.id WHERE b.tag IS NULL;
SELECT a.id, b.aid FROM a LEFT JOIN b ON NULL;
SELECT b.tag, a.name FROM b LEFT JOIN a ON a.id = b.aid;
