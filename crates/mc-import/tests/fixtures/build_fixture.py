#!/usr/bin/env python3
"""Build a minimal Magic Context-shaped SQLite fixture for mc-import tests.

Schema mirrors cortexkit/magic-context packages/plugin storage-db.ts
(LATEST_SUPPORTED_VERSION fence value recorded in the fixture; the
reader must refuse newer versions fail-closed):
- schema_migrations(version) — persisted version source (MAX(version)).
- session_projects(session_id, harness, project_path, updated_at).
- memories(id, project_path, category, content, normalized_hash,
  importance, scope, source_type, seen_count, retrieval_count,
  first_seen_at, created_at, updated_at, last_seen_at, status).
- memory_embeddings(memory_id, embedding BLOB, model_id).
Usage: build_fixture.py OUT.DB [--version N] [--wal]
"""
import hashlib
import sqlite3
import sys


def build(path: str, version: int, wal: bool) -> None:
    import os

    if os.path.exists(path):
        os.remove(path)
    db = sqlite3.connect(path)
    db.execute("CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY)")
    for v in (1, version):
        db.execute("INSERT OR IGNORE INTO schema_migrations(version) VALUES (?)", (v,))
    db.execute(
        """CREATE TABLE session_projects (
        session_id TEXT NOT NULL, harness TEXT NOT NULL DEFAULT 'opencode',
        project_path TEXT NOT NULL, updated_at INTEGER NOT NULL,
        PRIMARY KEY(session_id, harness))"""
    )
    db.execute(
        """CREATE TABLE memories (
        id INTEGER PRIMARY KEY AUTOINCREMENT, project_path TEXT NOT NULL,
        category TEXT NOT NULL, content TEXT NOT NULL, normalized_hash TEXT NOT NULL,
        importance INTEGER, scope TEXT NOT NULL DEFAULT 'project',
        source_type TEXT DEFAULT 'historian', seen_count INTEGER DEFAULT 1,
        retrieval_count INTEGER DEFAULT 0, first_seen_at INTEGER NOT NULL,
        created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
        last_seen_at INTEGER NOT NULL, status TEXT DEFAULT 'active')"""
    )
    db.execute(
        """CREATE TABLE memory_embeddings (
        memory_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
        embedding BLOB NOT NULL, model_id TEXT NOT NULL,
        PRIMARY KEY(memory_id, model_id))"""
    )
    content = "DECISION: Use Fjall for canonical storage."
    norm = hashlib.sha256(content.encode()).hexdigest()
    db.execute(
        """INSERT INTO memories(project_path, category, content, normalized_hash,
        importance, scope, source_type, first_seen_at, created_at, updated_at,
        last_seen_at, status) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)""",
        ("/repo/shop", "ARCHITECTURE", content, norm, 80, "project", "historian",
         1_700_000_000_000, 1_700_000_000_000, 1_700_000_000_000,
         1_700_000_000_000, "active"),
    )
    db.execute(
        "INSERT INTO session_projects VALUES (?,?,?,?)",
        ("sess-1", "opencode", "/repo/shop", 1_700_000_000_000),
    )
    db.commit()
    if wal:
        db.execute("PRAGMA journal_mode=WAL")
    db.close()


if __name__ == "__main__":
    out = sys.argv[1]
    version = 95
    wal = False
    for arg in sys.argv[2:]:
        if arg.startswith("--version="):
            version = int(arg.split("=", 1)[1])
        elif arg == "--wal":
            wal = True
    build(out, version, wal)
    print(f"wrote {out} version={version} wal={wal}")
