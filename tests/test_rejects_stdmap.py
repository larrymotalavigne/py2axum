"""Standard-library names outside the map are refused with their line and what to do instead (carte stdlib)."""
import sys

import pytest

from py2axum.__main__ import main

MAIN = '''
import array
import errno
import inspect
import itertools
import sys
import warnings

from fastapi import FastAPI

app = FastAPI()


@app.get("/x")
async def x(s: str = ""):
{body}
'''


@pytest.mark.parametrize("body, msg", [
    ('    warnings.warn("careful", stacklevel=2)\n    return {}', "warnings.warn(): only categories CPython's default filters ignore"),
    ('    warnings.warn("careful", type(ValueError()))\n    return {}', "only categories CPython's default filters ignore"),
    ('    warnings.warn("careful", category=UserWarning, stacklevel=2)\n    return {}', "use logging.warning()"),
    ('    warnings.simplefilter("error")\n    return {}', 'supported with the action "ignore" only'),
    ('    warnings.filterwarnings("always")\n    return {}', 'supported with the action "ignore" only'),
    ('    with warnings.catch_warnings(record=True):\n        return {}', "catch_warnings(record=True"),
    ('    return {"v": dir(s)}', "the builtin dir() is not supported"),
    ('    return {"v": inspect.signature(len)}', "the binary keeps no function objects to reflect on"),
    ('    return {"v": array.typecodes}', "array.array is in the map (numbers"),
    ('    return {"v": errno.EAGAIN}', "errno.EAGAIN"),
    ('    it = itertools.chain(s, s)\n    return {"v": next(it)}',
     "supported consumed whole only (list(), tuple(), set(), sorted(), str.join(), dict.fromkeys())"),
])
def test_stdlib_outside_map_rejected(tmp_path, capsys, body, msg):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(MAIN.format(body=body))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert msg in err, err
    assert "main.py:" in err, err


@pytest.mark.parametrize("body", [
    '    warnings.warn("old", DeprecationWarning, stacklevel=2)\n    return {}',
    '    warnings.warn(DeprecationWarning("old"))\n    return {}',
    '    with warnings.catch_warnings():\n        warnings.simplefilter("ignore")\n        warnings.warn("old", PendingDeprecationWarning)\n    return {}',
    '    return {"v": [errno.ENOENT, errno.EPIPE, sys.byteorder]}',
])
def test_stdlib_in_map_generated(tmp_path, body):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(MAIN.format(body=body))
    assert main([str(pkg), "--root", str(tmp_path), "-o", str(tmp_path / "out")]) == 0


@pytest.mark.skipif(sys.version_info < (3, 15), reason="a project on Python 3.15 is only read by a transpiler on 3.15")
def test_array_typecodes_of_python_315_refused(tmp_path, capsys):
    pkg = tmp_path / "proj"
    pkg.mkdir()
    (pkg / "__init__.py").write_text("")
    (pkg / "main.py").write_text(MAIN.format(body='    return {"v": array.array("Zf", [1.5]).tolist()}'))
    assert main([str(pkg), "--root", str(tmp_path), "--python", "3.15.0", "-o", str(tmp_path / "out")]) == 1
    err = capsys.readouterr().err
    assert "typecodes e, Zf and Zd (Python 3.15) are not supported" in err, err
    assert "main.py:" in err, err
