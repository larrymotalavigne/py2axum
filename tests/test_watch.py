"""`py2axum watch`: change detection, copy of the changed generated files only, failed cycles kept apart."""
import io
import os
import time

from py2axum.watch import Watcher, diff, parser, snapshot, sync_tree
from tests.test_report import write_project


def test_snapshot_watches_python_and_lock_files(tmp_path):
    (tmp_path / "pkg").mkdir()
    (tmp_path / "pkg" / "a.py").write_text("x = 1\n")
    (tmp_path / "uv.lock").write_text("")
    (tmp_path / "README.md").write_text("")
    (tmp_path / ".venv").mkdir()
    (tmp_path / ".venv" / "b.py").write_text("")
    (tmp_path / "out").mkdir()
    (tmp_path / "out" / "c.py").write_text("")
    snap = snapshot(tmp_path, exclude=(tmp_path / "out",))
    assert sorted(os.path.relpath(p, tmp_path) for p in snap) == ["pkg/a.py", "uv.lock"]
    (tmp_path / "pkg" / "a.py").write_text("x = 22\n")
    assert diff(snap, snapshot(tmp_path, exclude=(tmp_path / "out",))) == [str(tmp_path / "pkg" / "a.py")]


def test_sync_tree_writes_only_what_changed(tmp_path):
    src, dst = tmp_path / "src", tmp_path / "dst"
    (src / "src").mkdir(parents=True)
    (src / "Cargo.toml").write_text("[package]\n")
    (src / "src" / "main.rs").write_text("fn main() {}\n")
    changed, digests = sync_tree(src, dst)
    assert changed == ["Cargo.toml", "src/main.rs"]
    (dst / "target").mkdir()
    (dst / "target" / "kept").write_text("")
    (dst / "src" / "stale.rs").write_text("")
    time.sleep(0.01)
    (src / "src" / "main.rs").write_text("fn main() { println!(); }\n")
    (dst / "Cargo.toml").write_text("[package]\n# cargo rewrote me\n")  # as cargo adds the crate to Cargo.lock
    old = (dst / "Cargo.toml").stat().st_mtime_ns
    changed, digests = sync_tree(src, dst, digests)
    assert changed == ["src/main.rs", "src/stale.rs"]
    assert (dst / "Cargo.toml").stat().st_mtime_ns == old  # unchanged since the last generation: left alone
    assert not (dst / "src" / "stale.rs").exists() and (dst / "target" / "kept").exists()
    assert sync_tree(src, dst, digests)[0] == []
    (src / "Cargo.toml").write_text("[package]\nname = 'x'\n")
    assert sync_tree(src, dst, digests)[0] == ["Cargo.toml"]


def watcher(tmp_path, *flags):
    pkg = write_project(tmp_path)
    args = parser().parse_args([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out"), "--no-build", *flags])
    log = io.StringIO()
    return Watcher(args, out=log), log, pkg


def test_cycles(tmp_path):
    w, log, pkg = watcher(tmp_path, "--python-side", "auto")
    assert w.cycle("initial build")
    assert (tmp_path / "out" / "Cargo.toml").exists()
    assert "generated file(s) changed" in log.getvalue()
    assert w.cycle("again")
    assert "no generated file changed: nothing to rebuild" in log.getvalue()
    users = pkg / "views" / "users.py"
    users.write_text(users.read_text().replace('{"count": 0}', '{"count": 1}'))
    log.truncate(0)
    assert w.cycle("changed")
    assert "1 generated file(s) changed" in log.getvalue()


def test_failed_translation_keeps_the_previous_crate(tmp_path):
    w, log, pkg = watcher(tmp_path)  # without --python-side auto, the refused routes stop the translation
    assert not w.cycle("initial build")
    out = log.getvalue()
    assert "translation failed" in out and "error[P2A0" in out and "= help:" in out
    assert not (tmp_path / "out").exists()
