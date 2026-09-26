# Migrations

Numbered SQL applied to this split's own SQLite databases.

- `0001_codegraph.sql` — code-graph schema for `codegraph.sqlite`. Embedded into
  `jeryu-codegraph` with `include_str!` and applied with `execute_batch` every
  time the store is opened. Every statement is `CREATE TABLE IF NOT EXISTS` /
  `CREATE INDEX IF NOT EXISTS` / `INSERT OR REPLACE`, so applying it repeatedly
  is idempotent. It has no rollback: the database is a rebuildable index, and
  recovery is deleting the file and re-indexing.

Add further numbered migrations here only with rollback, backfill, and
lock-safety notes in the same change.
