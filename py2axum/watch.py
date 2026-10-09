"""`py2axum watch`: regenerate, rebuild and restart the binary each time the Python project changes.

    py2axum watch <package> -o <out> [--root DIR] [--name NAME] [--python-side ...] [--run] [--release]

The Python sources under --root (and the lock files that fix library versions) are polled; a burst of saves
is debounced into one cycle. Each cycle generates the crate into a staging directory, then copies into <out>
only the files whose content changed: cargo's fingerprints stay valid, so `cargo build` recompiles the
generated crate alone (the dependencies stay built), and a change that alters no Rust file skips the build.
With --run, the binary is restarted after each successful build; a cycle that fails (refused construct,
compile error) prints the error and keeps the previous binary running.

No dependency: polling (every --interval seconds) instead of file system events, so it behaves the same on
macOS, Linux and in containers with mounted volumes.
"""
from __future__ import annotations

import argparse
import filecmp
import hashlib
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

WATCHED_NAMES = {"uv.lock", "pyproject.toml", "requirements.txt", "poetry.lock"}
SKIPPED_DIRS = {".git", ".venv", "venv", "__pycache__", "node_modules", "target", ".mypy_cache", ".pytest_cache",
                ".ruff_cache", ".tox"}


def snapshot(root: Path, exclude: tuple[Path, ...] = ()) -> dict[str, tuple[float, int]]:
    """mtime and size of every watched file under root (the output directory excluded)."""
    out: dict[str, tuple[float, int]] = {}
    excluded = {p.resolve() for p in exclude}
    for dirpath, dirnames, filenames in os.walk(root):
        d = Path(dirpath)
        dirnames[:] = [n for n in dirnames if n not in SKIPPED_DIRS and not n.startswith(".")
                       and (d / n).resolve() not in excluded]
        for n in filenames:
            if n.endswith(".py") or n in WATCHED_NAMES or (n.startswith("requirements") and n.endswith(".txt")):
                p = d / n
                try:
                    st = p.stat()
                except OSError:
                    continue
                out[str(p)] = (st.st_mtime, st.st_size)
    return out


def diff(old: dict, new: dict) -> list[str]:
    return sorted({k for k in old.keys() | new.keys() if old.get(k) != new.get(k)})


def sync_tree(src: Path, dst: Path, previous: dict[str, str] | None = None,
              keep: tuple[str, ...] = ("target",)) -> tuple[list[str], dict[str, str]]:
    """Make dst hold src's files, writing only those that changed, so that unchanged files keep their mtime
    (cargo's fingerprints). `previous`: the digests of the last generation copied; a file is written when its
    digest changed since then or dst lacks it, so a file cargo itself rewrites (Cargo.lock gets the crate's own
    entry) is not copied again at every cycle. Without `previous`, contents are compared with dst. `keep`:
    top-level entries of dst left alone (cargo's target directory). Returns the changed paths (relative to dst)
    and the digests to pass as `previous` next time."""
    changed = []
    digests: dict[str, str] = {}
    dst.mkdir(parents=True, exist_ok=True)
    for dirpath, _, filenames in os.walk(src):
        rel_dir = Path(dirpath).relative_to(src)
        for n in filenames:
            rel = str(rel_dir / n) if rel_dir != Path(".") else n
            s, d = src / rel, dst / rel
            digests[rel] = hashlib.sha256(s.read_bytes()).hexdigest()
            if d.is_file():
                if previous is not None and previous.get(rel) == digests[rel]:
                    continue
                if previous is None and filecmp.cmp(s, d, shallow=False):
                    continue
            d.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(s, d)
            changed.append(rel)
    for dirpath, dirnames, filenames in os.walk(dst):
        rel_dir = Path(dirpath).relative_to(dst)
        if rel_dir == Path("."):
            dirnames[:] = [n for n in dirnames if n not in keep]
        for n in filenames:
            rel = str(rel_dir / n) if rel_dir != Path(".") else n
            if rel not in digests and rel not in keep:
                (dst / rel).unlink()
                changed.append(rel)
    return sorted(changed), digests


class Watcher:
    def __init__(self, args: argparse.Namespace, out=sys.stderr):
        self.a = args
        self.out = out
        self.package = args.package.resolve()
        self.root = (args.root or args.package.resolve().parent).resolve()
        self.dest = args.out.resolve()
        self.name = args.name or f"{self.package.name}_axum"
        self.stage = Path(tempfile.mkdtemp(prefix="py2axum-watch-"))
        self.proc: subprocess.Popen | None = None
        self.built_once = False
        self.digests: dict[str, str] | None = None
        self.moved: set[str] = set()  # the routes --python-side auto left to Python at the last cycle

    # ------------------------------------------------------------------ output
    def say(self, msg: str) -> None:
        print(f"[py2axum watch {time.strftime('%H:%M:%S')}] {msg}", file=self.out, flush=True)

    # ------------------------------------------------------------------ one cycle
    def generate(self) -> bool:
        cmd = [sys.executable, "-m", "py2axum", str(self.package), "-o", str(self.stage / "crate"), "--name",
               self.name, "--root", str(self.root)]
        for ps in self.a.python_side:
            cmd += ["--python-side", ps]
        if self.a.no_stream:
            cmd.append("--no-stream")
        if self.a.allow_untested_versions:
            cmd.append("--allow-untested-versions")
        shutil.rmtree(self.stage / "crate", ignore_errors=True)
        t = time.monotonic()
        r = subprocess.run(cmd, capture_output=True, text=True, env=self._py_env())
        if r.returncode != 0:
            self.say(f"translation failed ({time.monotonic() - t:.1f} s): the previous binary keeps running")
            print(_relative(r.stderr.rstrip()), file=self.out, flush=True)
            return False
        self.say(f"translated in {time.monotonic() - t:.1f} s")
        moved = {ln for ln in r.stderr.splitlines() if ln.startswith("python-side (auto):")}
        for ln in sorted(moved - self.moved):
            print(_relative(ln), file=self.out, flush=True)
        for ln in sorted(self.moved - moved):
            print(_relative(ln.replace("python-side (auto):", "native again:", 1)), file=self.out, flush=True)
        self.moved = moved
        return True

    def _py_env(self) -> dict:
        env = dict(os.environ)
        here = str(Path(__file__).resolve().parents[1])  # py2axum importable even when run from a checkout
        env["PYTHONPATH"] = here + (os.pathsep + env["PYTHONPATH"] if env.get("PYTHONPATH") else "")
        return env

    def build(self) -> bool:
        cmd = ["cargo", "build", "--message-format=short", "--manifest-path", str(self.dest / "Cargo.toml")]
        if self.a.release:
            cmd.append("--release")
        t = time.monotonic()
        r = subprocess.run(cmd, capture_output=True, text=True)
        if r.returncode != 0:
            self.say(f"cargo build failed ({time.monotonic() - t:.1f} s): the previous binary keeps running")
            lines = [ln for ln in r.stderr.splitlines() if not ln.lstrip().startswith(("Compiling", "Blocking"))]
            print("\n".join(lines[-40:]), file=self.out, flush=True)
            return False
        self.say(f"built in {time.monotonic() - t:.1f} s")
        return True

    def binary(self) -> Path:
        target = Path(os.environ.get("CARGO_TARGET_DIR") or self.dest / "target")
        return target / ("release" if self.a.release else "debug") / self.name

    def stop(self) -> None:
        if self.proc is None or self.proc.poll() is not None:
            self.proc = None
            return
        self.proc.send_signal(signal.SIGTERM)
        try:
            self.proc.wait(timeout=self.a.stop_timeout)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        self.proc = None

    def start(self) -> None:
        self.stop()
        self.proc = subprocess.Popen([str(self.binary()), *self.a.binary_args])
        self.say(f"started {self.binary().name} (pid {self.proc.pid})")

    def cycle(self, why: str) -> bool:
        """Translate, copy what changed, build, restart. True when the binary is up to date."""
        self.say(why)
        if not self.generate():
            return False
        changed, self.digests = sync_tree(self.stage / "crate", self.dest, self.digests)
        if not changed and self.built_once:
            self.say("no generated file changed: nothing to rebuild")
            if self.a.run and (self.proc is None or self.proc.poll() is not None):
                self.start()
            return True
        if changed:
            self.say(f"{len(changed)} generated file(s) changed")
        if self.a.no_build:
            self.built_once = True
            return True
        if not self.build():
            return False
        self.built_once = True
        if self.a.run:
            self.start()
        return True

    # ------------------------------------------------------------------ the loop
    def run(self, max_cycles: int | None = None) -> int:
        def interrupt(*_):
            raise KeyboardInterrupt

        # SIGTERM (and SIGINT, which a shell ignores in background jobs) stop the binary and exit cleanly
        signal.signal(signal.SIGTERM, interrupt)
        signal.signal(signal.SIGINT, interrupt)
        exclude = (self.dest, self.stage)
        self.say(f"watching {_relative(str(self.root))} (Ctrl-C to stop)")
        seen = snapshot(self.root, exclude)
        ok = self.cycle("initial build")
        cycles = 1
        try:
            while max_cycles is None or cycles < max_cycles:
                time.sleep(self.a.interval)
                now = snapshot(self.root, exclude)
                if now == seen:
                    if self.a.run and self.proc is not None and self.proc.poll() is not None and ok:
                        self.say(f"the binary exited with status {self.proc.returncode}; waiting for a change")
                        self.proc = None
                    continue
                # debounce: wait until the files stop changing (editors save in several writes)
                while True:
                    time.sleep(self.a.debounce)
                    later = snapshot(self.root, exclude)
                    if later == now:
                        break
                    now = later
                files = diff(seen, now)
                seen = now
                shown = ", ".join(_relative(f) for f in files[:3]) + (f" (+{len(files) - 3})" if len(files) > 3 else "")
                ok = self.cycle(f"changed: {shown}")
                cycles += 1
        except KeyboardInterrupt:
            pass
        finally:
            self.stop()
            shutil.rmtree(self.stage, ignore_errors=True)
            self.say("stopped")
        return 0 if ok else 1


def _relative(text: str) -> str:
    return text.replace(str(Path.cwd()) + "/", "")


def parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(prog="py2axum watch", description=__doc__.splitlines()[0])
    ap.add_argument("package", type=Path, help="directory of the FastAPI application package")
    ap.add_argument("-o", "--out", type=Path, required=True, help="output directory of the Rust project")
    ap.add_argument("--root", type=Path, default=None,
                    help="import root, like sys.path (default: parent of the package); the directory watched")
    ap.add_argument("--name", default=None, help="crate name (default: <package>_axum)")
    ap.add_argument("--python-side", action="append", default=[], metavar="PATH|lifespan|mount|auto",
                    help="as for generation (auto is handy while porting: refused routes do not stop the cycle)")
    ap.add_argument("--no-stream", action="store_true", help="buffer list responses instead of streaming them")
    ap.add_argument("--allow-untested-versions", action="store_true",
                    help="translate even if the project locks library versions outside the tested ranges")
    ap.add_argument("--run", action="store_true", help="start the binary, and restart it after each successful build")
    ap.add_argument("--release", action="store_true",
                    help="build the release profile (slow: fat LTO); default: the debug profile, incremental")
    ap.add_argument("--no-build", action="store_true", help="regenerate only (an IDE or another tool builds)")
    ap.add_argument("--interval", type=float, default=0.5, metavar="S", help="polling interval (default 0.5 s)")
    ap.add_argument("--debounce", type=float, default=0.3, metavar="S",
                    help="quiet time after the last change before a cycle starts (default 0.3 s)")
    ap.add_argument("--stop-timeout", type=float, default=5.0, metavar="S",
                    help="time the binary gets to exit after SIGTERM before it is killed (default 5 s)")
    ap.add_argument("binary_args", nargs="*", metavar="-- ARG", help="arguments passed to the binary, after --")
    return ap


def main(argv: list[str]) -> int:
    args = parser().parse_args(argv)
    if not args.package.is_dir():
        print(f"error: {args.package} is not a directory", file=sys.stderr)
        return 2
    if args.run and args.no_build:
        print("error: --run needs a build (drop --no-build)", file=sys.stderr)
        return 2
    if not args.no_build and shutil.which("cargo") is None:
        print("error: cargo is not on PATH (install Rust: https://rustup.rs), or pass --no-build", file=sys.stderr)
        return 2
    return Watcher(args).run()
