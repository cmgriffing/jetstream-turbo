import argparse
import contextlib
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import migrate_storage as migration


class MigrationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source = self.root / "legacy.db"
        with migration.writable(self.source) as db:
            db.executescript("""
                CREATE TABLE records(id INTEGER PRIMARY KEY, at_uri TEXT, did TEXT, time_us INTEGER,
                    source_event_id TEXT, message TEXT, message_metadata TEXT, created_at TEXT,
                    hydrated_at TEXT, hydration_quality TEXT DEFAULT 'unknown');
                CREATE TABLE publication_journal(source_event_id TEXT PRIMARY KEY, at_uri TEXT, created_at TEXT);
                CREATE TABLE ingestion_checkpoint(singleton_id INTEGER PRIMARY KEY, ingress_ordinal INTEGER,
                    time_us INTEGER, source_seq INTEGER, source_event_id TEXT, updated_at TEXT);
                INSERT INTO ingestion_checkpoint VALUES(1, 3, 3000000, 3, 'published', '2026-01-01T00:00:00Z');
                INSERT INTO records VALUES(1, 'at://pending', 'did:pending', 1000000, 'pending', '{}', '{"pending":true}', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 'unknown');
                INSERT INTO records VALUES(2, 'at://published', 'did:published', 2000000, 'published', '{}', '{}', '2026-01-01T00:00:01Z', '2026-01-01T00:00:01Z', 'unknown');
                INSERT INTO records VALUES(3, 'at://ambiguous', 'did:ambiguous', 4000000, NULL, '{}', '{}', '2026-01-01T00:00:02Z', '2026-01-01T00:00:02Z', 'unknown');
                INSERT INTO records VALUES(4, 'at://ahead', 'did:ahead', 9000000, 'ahead', '{}', '{}', '2026-01-01T00:00:03Z', '2026-01-01T00:00:03Z', 'unknown');
                INSERT INTO publication_journal VALUES('pending', 'at://pending', '2026-01-01T00:00:00Z');
                CREATE TABLE profile_cache(entity_key TEXT PRIMARY KEY, payload TEXT);
                INSERT INTO profile_cache VALUES('did:cached', '{}');
            """)
        self.args = argparse.Namespace(source=str(self.source), working=str(self.root / "working.db"), output=str(self.root / "replacement.db"), target_mb=10, writers_stopped=True)

    def migrate(self):
        with contextlib.redirect_stdout(io.StringIO()):
            migration.migrate(self.args)
        return Path(self.args.output + ".manifest.json")

    def test_migration_preserves_pending_ambiguous_checkpoint_and_uncovered_ahead_receipts(self):
        with migration.readonly(self.source) as db:
            before = migration.signature(db)
        manifest_path = self.migrate()
        with migration.readonly(Path(self.args.output)) as db:
            self.assertEqual(migration.signature(db), before)
            self.assertEqual(db.execute("SELECT source_event_id FROM records ORDER BY id").fetchall(), [("pending",), (None,)])
            self.assertEqual(db.execute("SELECT source_event_id, covered, ingress_epoch FROM completion_receipts ORDER BY source_event_id").fetchall(), [("ahead", 0, None), ("published", 0, None)])
            self.assertEqual(db.execute("SELECT count(*) FROM profile_cache").fetchone(), (0,))
        migration.verified_output(manifest_path)
        with migration.readonly(self.source) as db:
            self.assertEqual(db.execute("SELECT count(*) FROM records").fetchone(), (4,))

    def test_install_requires_verified_unchanged_state_and_rejects_sidecars(self):
        manifest_path = self.migrate()
        args = argparse.Namespace(manifest=str(manifest_path), destination=str(self.source), rollback=str(self.root / "rollback.db"), min_free_mb=0, writers_stopped=True)
        sidecar = Path(str(self.source) + "-wal")
        sidecar.write_text("stale")
        with self.assertRaisesRegex(ValueError, "sidecar"):
            migration.install(args)
        sidecar.unlink()
        with migration.writable(self.source) as db:
            db.execute("UPDATE ingestion_checkpoint SET ingress_ordinal=4")
        with self.assertRaisesRegex(ValueError, "changed"):
            migration.install(args)
        self.assertFalse(Path(args.rollback).exists())

    def test_atomic_install_preserves_rollback_and_refuses_tampering(self):
        manifest_path = self.migrate()
        output = Path(self.args.output)
        with migration.writable(output) as db:
            db.execute("UPDATE ingestion_checkpoint SET time_us=42")
        with self.assertRaisesRegex(ValueError, "verification failed"):
            migration.verified_output(manifest_path)
        with migration.writable(output) as db:
            db.execute("UPDATE ingestion_checkpoint SET time_us=3000000")
        # Recreate the pristine output after demonstrating checksum refusal;
        # restoring a single field does not promise byte-identical SQLite pages.
        output.unlink()
        Path(self.args.working).unlink()
        manifest_path.unlink()
        manifest_path = self.migrate()
        args = argparse.Namespace(manifest=str(manifest_path), destination=str(self.source), rollback=str(self.root / "rollback.db"), min_free_mb=0, writers_stopped=True)
        with contextlib.redirect_stdout(io.StringIO()):
            migration.install(args)
        with migration.readonly(self.source) as db:
            self.assertEqual(db.execute("SELECT count(*) FROM records").fetchone(), (2,))
        with migration.readonly(Path(args.rollback)) as db:
            self.assertEqual(db.execute("SELECT count(*) FROM records").fetchone(), (4,))

    def test_interrupted_install_before_replace_leaves_original_and_rollback(self):
        manifest_path = self.migrate()
        args = argparse.Namespace(manifest=str(manifest_path), destination=str(self.source), rollback=str(self.root / "rollback.db"), min_free_mb=0, writers_stopped=True)
        with patch.object(migration.os, "replace", side_effect=OSError("interrupted")):
            with self.assertRaisesRegex(OSError, "interrupted"):
                migration.install(args)
        with migration.readonly(self.source) as db:
            self.assertEqual(db.execute("SELECT count(*) FROM records").fetchone(), (4,))
        self.assertTrue(Path(self.args.output).exists())
        self.assertEqual(self.source.stat().st_ino, Path(args.rollback).stat().st_ino)

    def test_capacity_failure_and_failed_integrity_leave_source_untouched(self):
        with patch.object(migration.shutil, "disk_usage", return_value=argparse.Namespace(free=1)):
            with self.assertRaisesRegex(ValueError, "insufficient staging"):
                migration.migrate(self.args)
        self.assertFalse(Path(self.args.working).exists())
        with migration.writable(self.source) as db:
            db.execute("INSERT INTO publication_journal VALUES('orphan', 'at://orphan', 'old')")
        with self.assertRaisesRegex(ValueError, "without recoverable payloads"):
            migration.migrate(self.args)
        self.assertFalse(Path(self.args.output).exists())

    def test_legacy_without_journal_keeps_ambiguous_publication_status(self):
        with migration.writable(self.source) as db:
            db.execute("DROP TABLE publication_journal")
        self.migrate()
        with migration.readonly(Path(self.args.output)) as db:
            self.assertEqual(db.execute("SELECT count(*) FROM records").fetchone(), (4,))
            self.assertEqual(db.execute("SELECT count(*) FROM completion_receipts").fetchone(), (0,))

    def test_real_target_overage_keeps_protected_payload_and_refuses_manifest(self):
        with migration.writable(self.source) as db:
            db.execute("UPDATE records SET message=? WHERE source_event_id='pending'", ('{"large":"' + ('x' * (2 * migration.MIB)) + '"}',))
        self.args.target_mb = 1
        with self.assertRaisesRegex(ValueError, "protected state exceeds target"):
            migration.migrate(self.args)
        self.assertFalse(Path(self.args.output + ".manifest.json").exists())
        with migration.readonly(self.source) as db:
            self.assertEqual(db.execute("SELECT count(*) FROM records").fetchone(), (4,))

    def test_temp_spills_use_measured_private_staging_and_restore_environment(self):
        observed = []
        original_readonly = migration.readonly

        @contextlib.contextmanager
        def inspect_temp(path):
            temporary = Path(os.environ["SQLITE_TMPDIR"])
            self.assertTrue(temporary.is_dir())
            self.assertEqual(temporary.parent, self.root.resolve())
            observed.append(temporary)
            with original_readonly(path) as db:
                yield db

        with patch.dict(os.environ, {"SQLITE_TMPDIR": "/unwritable-host-temp"}):
            with patch.object(migration, "readonly", side_effect=inspect_temp):
                manifest = self.migrate()
                migration.verified_output(manifest, self.root)
            self.assertEqual(os.environ["SQLITE_TMPDIR"], "/unwritable-host-temp")
        self.assertGreaterEqual(len(observed), 3)
        self.assertTrue(all(not path.exists() for path in observed))

    def test_sort_headroom_is_required_before_migration_verification_and_install(self):
        size = self.source.stat().st_size
        with patch.object(migration.shutil, "disk_usage", return_value=argparse.Namespace(free=2 * size + migration.EMERGENCY_BYTES)):
            with self.assertRaisesRegex(ValueError, "insufficient staging"):
                migration.migrate(self.args)
        self.assertFalse(Path(self.args.working).exists())
        manifest = self.migrate()
        args = argparse.Namespace(manifest=str(manifest), destination=str(self.source), rollback=str(self.root / "rollback.db"), min_free_mb=0, writers_stopped=True)
        with patch.object(migration.shutil, "disk_usage", return_value=argparse.Namespace(free=0)):
            with self.assertRaisesRegex(ValueError, "SQLite temp staging capacity"):
                migration.verified_output(manifest)
            with self.assertRaisesRegex(ValueError, "SQLite temp staging capacity"):
                migration.install(args)
        self.assertFalse(Path(args.rollback).exists())
        self.assertTrue(Path(self.args.output).exists())

    def test_dangling_sidecar_symlink_blocks_installation(self):
        manifest = self.migrate()
        sidecar = Path(str(self.source) + "-wal")
        sidecar.symlink_to(self.root / "missing-wal-target")
        args = argparse.Namespace(manifest=str(manifest), destination=str(self.source), rollback=str(self.root / "rollback.db"), min_free_mb=0, writers_stopped=True)
        with self.assertRaisesRegex(ValueError, "sidecar"):
            migration.install(args)
        self.assertFalse(Path(args.rollback).exists())
        self.assertTrue(sidecar.is_symlink())

    def test_legacy_missing_journal_peers_are_repaired_and_never_compacted(self):
        timestamp = "2026-01-01T00:00:00Z"
        with migration.writable(self.source) as db:
            # Simulate origin/main's chunk bug: these payloads were stored with
            # the journaled first row, but publication never completed.
            db.execute("UPDATE records SET hydrated_at=? WHERE source_event_id='published'", (timestamp,))
        with migration.readonly(self.source) as db:
            source_expected = migration.signature(db)
        manifest_path = self.migrate()
        manifest = json.loads(manifest_path.read_text())
        self.assertEqual(manifest["format_version"], 2)
        self.assertEqual(manifest["repaired_journal_entries"], 1)
        self.assertEqual(manifest["source_expected"], source_expected)
        self.assertNotEqual(manifest["expected"]["journal"], source_expected["journal"])
        with migration.readonly(Path(self.args.output)) as db:
            self.assertEqual(db.execute("SELECT source_event_id FROM publication_journal ORDER BY source_event_id").fetchall(), [("pending",), ("published",)])
            self.assertIsNotNone(db.execute("SELECT message_metadata FROM records WHERE source_event_id='published'").fetchone())
            self.assertEqual(db.execute("SELECT COUNT(*) FROM completion_receipts WHERE source_event_id='published'").fetchone(), (0,))
        # Installation checks the original sparse source journal, whereas
        # replacement verification checks the normalized complete journal.
        args = argparse.Namespace(manifest=str(manifest_path), destination=str(self.source), rollback=str(self.root / "rollback.db"), min_free_mb=0, writers_stopped=True)
        with contextlib.redirect_stdout(io.StringIO()):
            migration.install(args)
        with migration.readonly(Path(args.rollback)) as db:
            self.assertEqual(db.execute("SELECT COUNT(*) FROM publication_journal").fetchone(), (1,))

    def test_ambiguous_timestamps_keep_payloads_and_get_conservative_journal(self):
        with migration.writable(self.source) as db:
            db.execute("UPDATE records SET hydrated_at='not-a-time' WHERE source_event_id='published'")
        self.migrate()
        with migration.readonly(Path(self.args.output)) as db:
            self.assertEqual(db.execute("SELECT COUNT(*) FROM records WHERE source_event_id='published'").fetchone(), (1,))
            self.assertEqual(db.execute("SELECT COUNT(*) FROM publication_journal WHERE source_event_id='published'").fetchone(), (1,))
            self.assertEqual(db.execute("SELECT COUNT(*) FROM completion_receipts WHERE source_event_id='published'").fetchone(), (0,))

    def test_pre_repair_manifest_format_is_never_installable(self):
        manifest_path = self.migrate()
        manifest = json.loads(manifest_path.read_text())
        manifest["format_version"] = 1
        manifest_path.write_text(json.dumps(manifest))
        with self.assertRaisesRegex(ValueError, "unsupported verification manifest"):
            migration.verified_output(manifest_path)

    def test_target_failure_never_produces_install_manifest(self):
        self.args.target_mb = 0
        with self.assertRaisesRegex(ValueError, "positive"):
            migration.migrate(self.args)
        self.assertFalse(Path(self.args.output + ".manifest.json").exists())


if __name__ == "__main__":
    unittest.main()
