"""The Unicode tables of a generated crate follow the project's Python, not the translating one
(py2axum/ucdages.py, tools/gen_ucdages.py): a character a later Unicode assigned is unassigned for the project."""
import ast
import sys
import unicodedata

import pytest

from py2axum import dyn, ucdages


def test_table_covers_supported_pythons():
    assert set(ucdages.UNICODE) == {(3, 12), (3, 13), (3, 14), (3, 15)}
    mine = sys.version_info[:2]
    if mine in ucdages.UNICODE:
        assert ucdages.UNICODE[mine] == unicodedata.unidata_version


def test_same_python_adds_nothing():
    assert dyn.unassigned_later(sys.version_info[:2]) == frozenset()


@pytest.mark.skipif(sys.version_info < (3, 14), reason="needs a translating Python with Unicode 16")
def test_older_project_sees_later_characters_unassigned():
    # U+0897 ARABIC PEPET (Unicode 16.0, a combining mark of class 230 in 3.14): unassigned on 3.12 and 3.13
    assert unicodedata.combining("ࢗ") == 230
    for python in ((3, 12), (3, 13)):
        assert 0x897 in dyn.unassigned_later(python)
        table = dyn.ucd_unassigned(python).split("= &[", 1)[1].rsplit("];", 1)[0]
        assert any(lo <= 0x897 <= hi for lo, hi in ast.literal_eval(f"[{table}]"))
    # CJK Unified Ideographs Extension I (Unicode 15.1): letters from 3.13, unassigned on 3.12
    assert "\U0002ebf0".isalpha()
    assert 0x2EBF0 in dyn.unassigned_later((3, 12)) and 0x2EBF0 not in dyn.unassigned_later((3, 13))
    alpha_312 = dyn.str_classes((3, 12)).split("alpha: ", 1)[1].split("]", 1)[0]
    alpha_313 = dyn.str_classes((3, 13)).split("alpha: ", 1)[1].split("]", 1)[0]
    assert alpha_312 != alpha_313
