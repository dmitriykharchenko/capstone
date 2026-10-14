import sqlite3
import tempfile
import unittest
from contextlib import closing
from pathlib import Path

from src.cli.commands import dispatch, handle_paper_metadata
from src.cli.config import CLIConfig


def config(tmp_path):
    return CLIConfig(
        database_path=tmp_path / "history.db",
        papers_directory=tmp_path / "papers",
        chroma_directory=tmp_path / "chroma",
        chat_model="unused",
        chat_base_url="http://unused.invalid",
        embedding_model="unused",
        embedding_base_url="http://unused.invalid",
        api_key=None,
        system_prompt=None,
    )


def seed_metadata(settings):
    settings.chroma_directory.mkdir()
    database = settings.chroma_directory / "chroma.sqlite3"
    with closing(sqlite3.connect(database)) as connection:
        connection.execute("CREATE TABLE embeddings (id INTEGER PRIMARY KEY, embedding_id TEXT)")
        connection.execute(
            "CREATE TABLE embedding_metadata (id INTEGER, key TEXT, string_value TEXT)"
        )
        for record_id in (1, 2):
            connection.execute(
                "INSERT INTO embeddings VALUES (?, ?)", (record_id, f"paper-a:{record_id}")
            )
            for key, value in (
                ("paper_id", "paper-a"),
                ("title", "A Stored Title"),
                ("authors", '["Ada Lovelace","Grace Hopper"]'),
                ("topics", '["program synthesis","verification"]'),
            ):
                connection.execute(
                    "INSERT INTO embedding_metadata VALUES (?, ?, ?)",
                    (record_id, key, value),
                )
        connection.commit()


class PaperMetadataCommandTests(unittest.TestCase):
    def setUp(self):
        self.temporary_directory = tempfile.TemporaryDirectory()
        self.settings = config(Path(self.temporary_directory.name))

    def tearDown(self):
        self.temporary_directory.cleanup()

    def test_reads_and_deduplicates_stored_metadata_without_chromadb(self):
        seed_metadata(self.settings)

        result = dispatch(
            "paper_metadata",
            self.settings,
            {"paper_ids": ["missing", "paper-a", "paper-a"]},
        )

        self.assertEqual(
            result,
            {
                "available": True,
                "papers": [
                    {
                        "paper_id": "paper-a",
                        "title": "A Stored Title",
                        "authors": ["Ada Lovelace", "Grace Hopper"],
                        "topics": ["program synthesis", "verification"],
                    }
                ],
            },
        )

    def test_missing_store_is_reported_without_creating_files(self):
        self.assertEqual(
            handle_paper_metadata(self.settings, {"paper_ids": ["paper-a"]}),
            {
                "available": False,
                "papers": [],
                "reason": "indexed paper metadata unavailable",
            },
        )
        self.assertFalse(self.settings.chroma_directory.exists())


if __name__ == "__main__":
    unittest.main()
