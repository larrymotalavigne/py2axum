"""[tool.py2axum] in pyproject.toml (py2axum/config.py): the options put in front of the command line."""
import pytest

from py2axum import check, watch
from py2axum.__main__ import parser as gen_parser
from py2axum.config import ConfigError, apply


def write(tmp_path, body):
    (tmp_path / "pyproject.toml").write_text('[project]\nname = "x"\n\n[tool.py2axum]\n' + body)


def test_options_and_package(tmp_path):
    write(tmp_path, 'package = "app"\nroot = "."\npython_side = ["/a", "lifespan"]\nallow-untested-versions = true\n'
                    'fail-under = 90\nname = "x_axum"\n')
    argv = apply(["--format", "json"], check.parser, start=tmp_path)
    assert argv == [str(tmp_path / "app"), "--root", str(tmp_path), "--python-side", "/a", "--python-side", "lifespan",
                    "--allow-untested-versions", "--fail-under", "90", "--format", "json"]
    args = check.parser().parse_args(argv)
    assert args.python_side == ["/a", "lifespan"] and args.fail_under == 90 and args.format == "json"
    # generation: fail-under is check's, skipped; name is generation's
    argv = apply(["-o", "out"], gen_parser, start=tmp_path)
    assert "--fail-under" not in argv and argv[-4:] == ["--name", "x_axum", "-o", "out"]
    assert gen_parser().parse_args(argv).package == tmp_path / "app"


def test_command_line_wins(tmp_path):
    write(tmp_path, 'package = "app"\napp = "app.main:app"\npython-side = "/a"\n')
    args = check.parser().parse_args(apply(["other", "--app", "other.main:app", "--python-side", "/b"], check.parser,
                                           start=tmp_path))
    # the package and a scalar given on the command line win; a repeatable option adds to the file's
    assert str(args.package) == "other" and args.app == "other.main:app" and args.python_side == ["/a", "/b"]


def test_closest_table_from_a_subdirectory(tmp_path):
    write(tmp_path, 'root = "src"\n')
    sub = tmp_path / "src" / "app"
    sub.mkdir(parents=True)
    (tmp_path / "src" / "pyproject.toml").write_text('[project]\nname = "inner"\n')  # no table: skipped
    assert apply(["app"], check.parser, start=sub) == ["--root", str(tmp_path / "src"), "app"]


def test_watch_options(tmp_path):
    write(tmp_path, 'package = "app"\nout = "gen"\nrun = true\ninterval = 2\nfail-under = 50\n')
    args = watch.parser().parse_args(apply([], watch.parser, start=tmp_path))
    assert args.out == tmp_path / "gen" and args.run and args.interval == 2


def test_no_table(tmp_path):
    (tmp_path / "pyproject.toml").write_text('[project]\nname = "x"\n')
    assert apply(["app"], check.parser, start=tmp_path) == ["app"]


@pytest.mark.parametrize("body, msg", [
    ('bogus = 1\n', "unknown key 'bogus'"),
    ('explain = "P2A0201"\n', "explain is a command-line option only"),
    ('allow-untested-versions = "yes"\n', "must be true or false"),
    ('app = ["a", "b"]\n', "app must be a string or a number, not list"),
])
def test_errors(tmp_path, body, msg):
    write(tmp_path, body)
    with pytest.raises(ConfigError, match=msg):
        apply([], check.parser, start=tmp_path)
