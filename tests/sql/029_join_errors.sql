CREATE TABLE a (id INTEGER, v TEXT);
CREATE TABLE b (id INTEGER, w TEXT);
SELECT * FROM a JOIN a ON a.id = a.id;
SELECT id FROM a JOIN b ON a.id = b.id;
SELECT a.v FROM a JOIN b ON c.id = a.id;
SELECT a.v FROM a JOIN b ON a.id = b.nope;
SELECT * FROM a JOIN b ON a.id = b.id JOIN missing ON TRUE;
SELECT x.v FROM a AS x JOIN b ON a.id = b.id;
