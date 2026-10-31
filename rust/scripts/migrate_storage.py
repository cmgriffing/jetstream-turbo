#!/usr/bin/env python3
"""Offline, conservative SQLite compaction. Never starts ingestion or modifies source."""
import argparse
import contextlib
from datetime import datetime
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import sys
import tempfile

MIB = 1024 * 1024
EMERGENCY_BYTES = 2048 * MIB
RECORD_INDEXES = {
    "idx_records_at_uri": "CREATE INDEX idx_records_at_uri ON records(at_uri)",
    "idx_records_did": "CREATE INDEX idx_records_did ON records(did)",
    "idx_records_time_us": "CREATE INDEX idx_records_time_us ON records(time_us)",
    "idx_records_created_at": "CREATE INDEX idx_records_created_at ON records(created_at)",
    "idx_records_hydration_quality": "CREATE INDEX idx_records_hydration_quality ON records(hydration_quality)",
    "idx_records_source_event_id": "CREATE UNIQUE INDEX idx_records_source_event_id ON records(source_event_id) WHERE source_event_id IS NOT NULL",
}
RECEIPT_SCHEMA = """
CREATE TABLE IF NOT EXISTS recovery_admissions (
 source_event_id TEXT PRIMARY KEY, source_time_us INTEGER NOT NULL,
 ingress_epoch TEXT NOT NULL, ingress_ordinal INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS completion_receipts (
 source_event_id TEXT PRIMARY KEY, source_time_us INTEGER,
 ingress_epoch TEXT, ingress_ordinal INTEGER,
 covered INTEGER NOT NULL DEFAULT 0 CHECK(covered IN (0, 1)), completed_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS idx_completion_receipts_coverage
 ON completion_receipts(ingress_epoch, covered, ingress_ordinal);
CREATE INDEX IF NOT EXISTS idx_completion_receipts_expiry
 ON completion_receipts(covered, source_time_us);
"""


@contextlib.contextmanager
def readonly(path):
    db = sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True)
    try:
        yield db
    finally:
        db.close()


@contextlib.contextmanager
def writable(path):
    db = sqlite3.connect(path)
    try:
        with db:
            yield db
    finally:
        db.close()


def tables(db):
    return {row[0] for row in db.execute("SELECT name FROM sqlite_schema WHERE type='table'")}


def columns(db, table):
    return {row[1] for row in db.execute(f"PRAGMA table_info({table})")}


def digest(rows):
    result = hashlib.sha256()
    count = 0
    for row in rows:
        payload = json.dumps(list(row), ensure_ascii=False, separators=(",", ":")).encode()
        result.update(len(payload).to_bytes(8, "big"))
        result.update(payload)
        count += 1
    return {"count": count, "sha256": result.hexdigest()}


def file_digest(path):
    result = hashlib.sha256()
    with path.open("rb") as data:
        for chunk in iter(lambda: data.read(MIB), b""):
            result.update(chunk)
    return result.hexdigest()


def sync_file(path):
    with path.open("rb") as data:
        os.fsync(data.fileno())


def valid_timestamp(value):
    if not isinstance(value, str) or len(value) > 64:
        return False
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
        return parsed.tzinfo is not None and parsed.utcoffset() is not None
    except ValueError:
        return False


def protected_predicate(db):
    present = tables(db)
    record_columns = columns(db, "records")
    if "publication_journal" not in present or "source_event_id" not in record_columns:
        return "1"
    if "hydrated_at" not in record_columns:
        return "1"
    db.create_function("recovery_timestamp_valid", 1, valid_timestamp, deterministic=True)
    # The legacy single-VALUES journal bug wrote only one identity per SQL chunk.
    # All rows in that batch share hydrated_at with surviving journal.created_at.
    # Timestamp membership protects omitted peers without per-row journal scans.
    return """source_event_id IS NULL
        OR NOT recovery_timestamp_valid(hydrated_at)
        OR source_event_id IN (SELECT source_event_id FROM publication_journal)
        OR hydrated_at IN (SELECT created_at FROM publication_journal)"""


def repair_legacy_publication_journal(db):
    """Rebuild conservative pending peers on the working copy, never the source."""
    db.create_function("recovery_timestamp_valid", 1, valid_timestamp, deterministic=True)
    result = db.execute("""INSERT OR IGNORE INTO publication_journal(source_event_id, at_uri, created_at)
        SELECT source_event_id, at_uri, COALESCE(hydrated_at, strftime('%Y-%m-%dT%H:%M:%fZ','now'))
        FROM records WHERE source_event_id IS NOT NULL
        AND (NOT recovery_timestamp_valid(hydrated_at)
             OR hydrated_at IN (SELECT created_at FROM publication_journal))""")
    return result.rowcount


def signature(db, protected_columns=None):
    present = tables(db)
    if "records" not in present:
        raise ValueError("source has no records table")
    has_identity = "source_event_id" in columns(db, "records")
    identity_queries = []
    if has_identity:
        identity_queries.append("SELECT source_event_id FROM records WHERE source_event_id IS NOT NULL")
    for table in ("completion_receipts", "publication_journal"):
        if table in present:
            identity_queries.append(f"SELECT source_event_id FROM {table}")
    identities = digest(db.execute(" UNION ".join(identity_queries) + " ORDER BY 1")) if identity_queries else digest([])
    protected = protected_predicate(db)
    # Hash every original payload column, including metrics. A legacy copy may
    # gain compatibility columns; the manifest preserves its original projection.
    actual_columns = columns(db, "records")
    payload_columns = sorted(actual_columns) if protected_columns is None else protected_columns
    if not set(payload_columns).issubset(actual_columns):
        raise ValueError("protected payload projection does not match database schema")
    projection = ",".join('"' + name.replace('"', '""') + '"' for name in payload_columns)
    protected = digest(db.execute(f"SELECT {projection} FROM records WHERE {protected} ORDER BY id"))
    checkpoint = list(db.execute("SELECT * FROM ingestion_checkpoint ORDER BY singleton_id")) if "ingestion_checkpoint" in present else []
    journal = digest(db.execute("SELECT source_event_id, at_uri, created_at FROM publication_journal ORDER BY source_event_id")) if "publication_journal" in present else digest([])
    return {"identities": identities, "protected_payloads": protected, "protected_columns": payload_columns, "checkpoint": [list(row) for row in checkpoint], "journal": journal}


def normalize(sql):
    return " ".join(sql.lower().split()).replace(" if not exists", "").rstrip(";")


def verify_indexes(db):
    for name, expected in RECORD_INDEXES.items():
        row = db.execute("SELECT sql FROM sqlite_schema WHERE type='index' AND name=?", (name,)).fetchone()
        if row is None or normalize(row[0]) != normalize(expected):
            raise ValueError(f"missing or incompatible required index: {name}")
    for name, expected in {
        "idx_completion_receipts_coverage": "CREATE INDEX idx_completion_receipts_coverage ON completion_receipts(ingress_epoch, covered, ingress_ordinal)",
        "idx_completion_receipts_expiry": "CREATE INDEX idx_completion_receipts_expiry ON completion_receipts(covered, source_time_us)",
    }.items():
        row = db.execute("SELECT sql FROM sqlite_schema WHERE type='index' AND name=?", (name,)).fetchone()
        if row is None or normalize(row[0]) != normalize(expected):
            raise ValueError(f"missing or incompatible required index: {name}")


def integrity(db):
    result = list(db.execute("PRAGMA integrity_check"))
    if result != [("ok",)]:
        raise ValueError(f"integrity check failed: {result[:4]}")
    present = tables(db)
    if "publication_journal" in present and "source_event_id" in columns(db, "records"):
        missing = db.execute("SELECT count(*) FROM publication_journal j WHERE NOT EXISTS(SELECT 1 FROM records r WHERE r.source_event_id=j.source_event_id)").fetchone()[0]
        if missing:
            raise ValueError("publication journal contains identities without recoverable payloads")


def absent(path):
    if path.exists() or path.is_symlink():
        raise ValueError(f"refusing to overwrite existing path: {path}")


def no_sidecars(path):
    for suffix in ("-wal", "-shm", "-journal"):
        sidecar = Path(str(path) + suffix)
        if sidecar.exists() or sidecar.is_symlink():
            raise ValueError(f"sidecar present for {path}; stop/checkpoint/close all writers before installation")


def require_stopped(args):
    if not args.writers_stopped:
        raise ValueError("--writers-stopped is required; drain and stop every database writer first")


@contextlib.contextmanager
def sqlite_temp(directory, required_bytes):
    """Keep all SQLite sort/index temporaries on measured, writable staging."""
    directory = Path(directory).resolve()
    if not directory.is_dir():
        raise ValueError(f"SQLite temp staging directory does not exist: {directory}")
    available = shutil.disk_usage(directory).free
    if available < required_bytes:
        raise ValueError(f"insufficient SQLite temp staging capacity on {directory}: need {required_bytes} free bytes, observed {available}")
    # The CLI runs one offline operation at a time. Set this before opening any
    # SQLite connection; a private directory avoids collisions with other jobs.
    with tempfile.TemporaryDirectory(prefix=".jetstream-sqlite-temp-", dir=directory) as temporary:
        previous = os.environ.get("SQLITE_TMPDIR")
        os.environ["SQLITE_TMPDIR"] = temporary
        try:
            yield
        finally:
            if previous is None:
                os.environ.pop("SQLITE_TMPDIR", None)
            else:
                os.environ["SQLITE_TMPDIR"] = previous


def migrate(args):
    with sqlite_temp(Path(args.working).resolve().parent, 0):
        return _migrate(args)


def _migrate(args):
    require_stopped(args)
    source, working, output = (Path(getattr(args, key)).resolve() for key in ("source", "working", "output"))
    if len({source, working, output}) != 3:
        raise ValueError("source, working copy, and output must be different paths")
    if args.target_mb <= 0:
        raise ValueError("target must be positive")
    if not source.is_file():
        raise ValueError("source must be an existing database")
    for path in (working, output, Path(str(output) + ".manifest.json")):
        absent(path)
        if not path.parent.is_dir():
            raise ValueError(f"staging directory does not exist: {path.parent}")
    source_bytes = source.stat().st_size + sum(Path(str(source) + suffix).stat().st_size for suffix in ("-wal",) if Path(str(source) + suffix).exists())
    # Reserve a full working copy, rollback/index-build growth, and one source
    # allocation for verification sorting. SQLite temporaries are explicitly
    # directed to the working staging volume, including output verification.
    working_device = working.parent.stat().st_dev
    output_device = output.parent.stat().st_dev
    requirements = {working_device: [working.parent, 3 * source_bytes + EMERGENCY_BYTES]}
    if output_device != working_device:
        requirements[output_device] = [output.parent, source_bytes + EMERGENCY_BYTES]
    for directory, required in requirements.values():
        free = shutil.disk_usage(directory).free
        if free < required:
            raise ValueError(f"insufficient staging capacity on {directory}: need {required} free bytes, observed {free}; use alternate capacity or expand disk")
    with readonly(source) as original, writable(working) as db:
        original.backup(db)
        db.execute("PRAGMA journal_mode=DELETE")
        integrity(db)
        source_expected = signature(db)
        original_tables = tables(db)
        had_journal = "publication_journal" in original_tables
        record_columns = columns(db, "records")
        if "source_event_id" not in record_columns:
            db.execute("ALTER TABLE records ADD COLUMN source_event_id TEXT")
        if "hydration_quality" not in record_columns:
            db.execute("ALTER TABLE records ADD COLUMN hydration_quality TEXT NOT NULL DEFAULT 'unknown'")
        db.executescript(RECEIPT_SCHEMA)
        repaired_journal_entries = 0
        if had_journal:
            repaired_journal_entries = repair_legacy_publication_journal(db)
        expected = signature(db, source_expected["protected_columns"])
        for key in ("identities", "protected_payloads", "checkpoint"):
            if expected[key] != source_expected[key]:
                raise ValueError("legacy journal repair changed protected recovery state")
        if had_journal:
            # Never infer durable frontier coverage from wall time/source time.
            # Known published identities can shed payloads but remain uncovered.
            db.execute("""INSERT INTO completion_receipts(source_event_id,source_time_us,ingress_epoch,ingress_ordinal,covered,completed_at)
                SELECT source_event_id,time_us,NULL,NULL,0,strftime('%Y-%m-%dT%H:%M:%fZ','now') FROM records
                WHERE source_event_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM publication_journal j WHERE j.source_event_id=records.source_event_id)
                ON CONFLICT(source_event_id) DO NOTHING""")
            db.execute("""DELETE FROM records WHERE source_event_id IS NOT NULL
                AND EXISTS(SELECT 1 FROM completion_receipts c WHERE c.source_event_id=records.source_event_id)
                AND NOT EXISTS(SELECT 1 FROM publication_journal j WHERE j.source_event_id=records.source_event_id)""")
        for name in ("profile_cache", "post_cache"):
            if name in original_tables:
                db.execute(f"DELETE FROM {name}")
        for name, ddl in RECORD_INDEXES.items():
            existing = db.execute("SELECT sql FROM sqlite_schema WHERE type='index' AND name=?", (name,)).fetchone()
            if existing is None:
                db.execute(ddl)
            elif normalize(existing[0]) != normalize(ddl):
                raise ValueError(f"incompatible required index: {name}")
        db.commit()
        if signature(db, expected["protected_columns"]) != expected:
            raise ValueError("working-copy recovery verification failed; source remains unchanged")
        integrity(db)
        verify_indexes(db)
        db.execute("VACUUM INTO ?", (str(output),))
    manifest = {"format_version": 2, "source": str(source), "output": str(output), "target_bytes": args.target_mb * MIB, "source_expected": source_expected, "expected": expected, "repaired_journal_entries": repaired_journal_entries}
    manifest_path = Path(str(output) + ".manifest.json")
    with readonly(output) as compacted:
        integrity(compacted)
        verify_indexes(compacted)
        if signature(compacted, expected["protected_columns"]) != expected:
            raise ValueError("compacted recovery verification failed; source remains unchanged")
    if output.stat().st_size > manifest["target_bytes"]:
        raise ValueError("compacted protected state exceeds target; installation refused, expand capacity or resolve recovery backlog")
    manifest["output_sha256"] = file_digest(output)
    sync_file(output)
    with manifest_path.open("x") as f:
        json.dump(manifest, f, indent=2)
        f.write("\n")
        f.flush()
        os.fsync(f.fileno())
    sync_directory(output.parent)
    print(json.dumps({"status": "verified", "output": str(output), "bytes": output.stat().st_size, "manifest": str(manifest_path), "recovery": expected}))


def verified_output(manifest_path, temp_dir=None):
    manifest = json.loads(Path(manifest_path).read_text())
    output = Path(manifest["output"])
    staging = Path(temp_dir) if temp_dir else output.parent
    with sqlite_temp(staging, output.stat().st_size + EMERGENCY_BYTES):
        return _verified_output(manifest_path)


def _verified_output(manifest_path):
    manifest = json.loads(Path(manifest_path).read_text())
    if manifest.get("format_version") != 2:
        raise ValueError("unsupported verification manifest")
    output = Path(manifest["output"])
    no_sidecars(output)
    if file_digest(output) != manifest["output_sha256"]:
        raise ValueError("replacement verification failed: database checksum changed")
    with readonly(output) as db:
        integrity(db)
        verify_indexes(db)
        for table in ("profile_cache", "post_cache"):
            if table in tables(db) and db.execute(f"SELECT count(*) FROM {table}").fetchone()[0]:
                raise ValueError("entity caches must be empty for migration")
        if signature(db, manifest["expected"]["protected_columns"]) != manifest["expected"]:
            raise ValueError("checkpoint, protected payload, journal, or identity verification failed")
    if output.stat().st_size > manifest["target_bytes"]:
        raise ValueError("replacement does not fit aggregate target")
    return manifest, output


def verify(args):
    manifest, output = verified_output(args.manifest, getattr(args, "temp_dir", None))
    print(json.dumps({"status": "verified", "output": str(output), "recovery": manifest["expected"]}))


def sync_directory(path):
    fd = os.open(path, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def install(args):
    require_stopped(args)
    manifest = json.loads(Path(args.manifest).read_text())
    output = Path(manifest["output"])
    destination = Path(args.destination).resolve()
    staging = Path(args.temp_dir) if getattr(args, "temp_dir", None) else output.parent
    sort_bytes = max(output.stat().st_size, destination.stat().st_size if destination.exists() else 0)
    with sqlite_temp(staging, sort_bytes + EMERGENCY_BYTES):
        return _install(args)


def _install(args):
    require_stopped(args)
    if args.min_free_mb < 0:
        raise ValueError("free-space reserve cannot be negative")
    manifest, output = _verified_output(args.manifest)
    destination = Path(args.destination).resolve()
    if output == destination:
        raise ValueError("replacement and destination must differ")
    no_sidecars(destination)
    if destination.parent.stat().st_dev != output.stat().st_dev:
        raise ValueError("atomic installation requires a verified replacement staged on the destination filesystem; copy it and its manifest there, update output path, and verify again")
    free = shutil.disk_usage(destination.parent).free
    if free < args.min_free_mb * MIB:
        raise ValueError("destination lacks the configured post-install free-space reserve; relocate old database/backup off volume or expand disk")
    rollback = Path(args.rollback).resolve()
    absent(rollback)
    if rollback.parent.stat().st_dev != destination.parent.stat().st_dev:
        raise ValueError("rollback hardlink must be on the destination filesystem")
    sync_file(output)
    sync_directory(output.parent)
    if destination.exists():
        with readonly(destination) as current:
            if signature(current, manifest["source_expected"]["protected_columns"]) != manifest["source_expected"]:
                raise ValueError("destination recovery state changed since snapshot; stop writers and migrate again")
        os.link(destination, rollback)
        sync_directory(rollback.parent)
    # Crash before rename leaves the old destination; crash after rename leaves
    # the fully verified replacement and the retained old inode at rollback.
    os.replace(output, destination)
    sync_directory(destination.parent)
    print(json.dumps({"status": "installed", "destination": str(destination), "rollback": str(rollback) if rollback.exists() else None}))


def parser():
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest="command", required=True)
    m = sub.add_parser("migrate")
    for key in ("source", "working", "output"):
        m.add_argument("--" + key, required=True)
    m.add_argument("--target-mb", type=int, default=6144)
    m.add_argument("--writers-stopped", action="store_true")
    m.set_defaults(run=migrate)
    v = sub.add_parser("verify")
    v.add_argument("--manifest", required=True)
    v.add_argument("--temp-dir", help="Measured writable SQLite temp staging; defaults to replacement directory")
    v.set_defaults(run=verify)
    i = sub.add_parser("install")
    for key in ("manifest", "destination", "rollback"):
        i.add_argument("--" + key, required=True)
    i.add_argument("--min-free-mb", type=int, default=16384)
    i.add_argument("--temp-dir", help="Measured writable SQLite temp staging; defaults to replacement directory")
    i.add_argument("--writers-stopped", action="store_true")
    i.set_defaults(run=install)
    return p


def main():
    args = parser().parse_args()
    try:
        args.run(args)
    except (ValueError, OSError, sqlite3.Error, KeyError) as error:
        print(f"migration refused: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
