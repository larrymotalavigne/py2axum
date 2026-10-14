"""The source part of a traceback frame, as CPython's `traceback` prints it, computed at translation.

A frame of a traceback is `  File "<path>", line <n>, in <name>` followed by the source of the instruction
that was running (its line, or its lines from 3.13 on) and, when the instruction does not span the whole
line, carets under it (`~~^~~` under an operator, `~^^^^` under a call from 3.13 on). The binary prints
the first line with the path of the running deployment; the rest depends only on the source and on the
instruction's position, so it is computed here, once per position, by the `traceback` module of the
interpreter running py2axum (the same Python as the project's: 3.12 and 3.13+ differ, 3.13 and 3.14 agree).
"""
from __future__ import annotations

import traceback


def frame_text(path: str, lineno: int, end_lineno: int | None, col: int | None, end_col: int | None) -> str:
    """What follows the `File` line of a frame for an instruction at this position (byte columns, like
    CPython's code positions); "" when the source line is blank or missing."""
    fs = traceback.FrameSummary(path, lineno, "f", end_lineno=end_lineno, colno=col, end_colno=end_col)
    text = traceback.StackSummary.from_list([fs]).format_frame_summary(fs)
    return text.split("\n", 1)[1] if "\n" in text else ""
