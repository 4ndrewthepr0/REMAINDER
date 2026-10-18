const { DatabaseSync } = require('node:sqlite');
const db = new DatabaseSync('remainder.db');

console.log('--- DATABASE PRAGMAS ---');
console.log('user_version:', db.prepare('PRAGMA user_version').get());
console.log('journal_mode:', db.prepare('PRAGMA journal_mode').get());
console.log('foreign_keys:', db.prepare('PRAGMA foreign_keys').get());

console.log('\n--- TABLES ---');
console.log(db.prepare("SELECT name, sql FROM sqlite_master WHERE type='table'").all());

console.log('\n--- INDEXES ---');
console.log(db.prepare("SELECT name, sql FROM sqlite_master WHERE type='index'").all());

console.log('\n--- EXPLAIN QUERY PLAN ---');
console.log(db.prepare("EXPLAIN QUERY PLAN SELECT fact_content FROM atomics WHERE vault_tag = 'DATABASE' COLLATE NOCASE ORDER BY id DESC LIMIT 1").all());

console.log('\n--- RECORD COUNTS ---');
console.log('sources:', db.prepare("SELECT count(*) as c FROM sources").get());
console.log('atomics:', db.prepare("SELECT count(*) as c FROM atomics").get());

db.close();
