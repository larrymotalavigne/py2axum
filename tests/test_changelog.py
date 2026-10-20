"""tools/changelog.py: fragments in changelog.d/, [Unreleased] of CHANGELOG.md written only by the tool."""
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "tools"))
import changelog  # noqa: E402

LOG = """# Changelog

## [Unreleased]

### Added

- old added

### Fixed

- old fixed

## [1.0.0] — 2026-01-01

- first
"""


def test_repository_is_consistent():
    assert changelog.main(["check"]) == 0


@pytest.fixture
def tree(tmp_path, monkeypatch):
    (tmp_path / "changelog.d").mkdir()
    log = tmp_path / "CHANGELOG.md"
    log.write_text(LOG)
    monkeypatch.setattr(changelog, "CHANGELOG", log)
    monkeypatch.setattr(changelog, "FRAGMENTS", tmp_path / "changelog.d")
    monkeypatch.setattr(changelog, "STAMP", tmp_path / "changelog.d" / ".unreleased.sha256")
    monkeypatch.setattr(changelog, "ROOT", tmp_path)
    changelog.stamp(LOG)
    return tmp_path


def test_assemble_orders_sections_and_empties_folder(tree):
    d = tree / "changelog.d"
    (d / "a.fixed.md").write_text("- fix a\n  continued\n")
    (d / "b.security.md").write_text("- sec b\n")
    (d / "c.added.md").write_text("- add c\n")
    assert changelog.main(["check"]) == 0
    assert changelog.main(["assemble"]) == 0
    text = (tree / "CHANGELOG.md").read_text()
    unreleased = text[text.index("## [Unreleased]"):text.index("## [1.0.0]")]
    assert [ln for ln in unreleased.splitlines() if ln.startswith("###")] == ["### Security", "### Added", "### Fixed"]
    assert "- old added\n- add c" in unreleased and "- old fixed\n- fix a\n  continued" in unreleased
    assert text.endswith("- first\n") and "## [1.0.0]" in text
    assert not list(d.glob("*.md"))
    assert changelog.main(["check"]) == 0


def test_direct_edit_of_unreleased_is_refused(tree):
    log = tree / "CHANGELOG.md"
    log.write_text(LOG.replace("- old added", "- old added\n- sneaked in"))
    assert changelog.main(["check"]) == 1
    assert changelog.main(["assemble"]) == 1
    # editing a released section is not this check's business
    log.write_text(LOG.replace("- first", "- first, amended"))
    assert changelog.main(["check"]) == 0


def test_bad_fragment_names_and_bodies(tree):
    d = tree / "changelog.d"
    (d / "x.bogus.md").write_text("- a\n")
    (d / "y.added.md").write_text("no bullet\n")
    assert len(changelog.problems()) == 2
    assert changelog.main(["assemble"]) == 1
