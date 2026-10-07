CREATE TABLE a (id INTEGER PRIMARY KEY, name TEXT);
CREATE TABLE b (aid INTEGER, tag TEXT);
CREATE INDEX b_aid ON b (aid);
EXPLAIN SELECT a.name, b.tag FROM a JOIN b ON b.aid = a.id;
EXPLAIN SELECT a.name, b.tag FROM b LEFT JOIN a ON a.id = b.aid;
EXPLAIN SELECT a.name, b.tag FROM a JOIN b ON b.tag = a.name;
EXPLAIN SELECT a.name FROM a JOIN b ON b.aid = a.id WHERE a.id = 1;
