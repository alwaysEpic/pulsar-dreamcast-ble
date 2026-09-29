# SPDX-License-Identifier: GPL-3.0-or-later
"""Make ``vmu_read`` importable regardless of pytest's invocation directory.

The script imports ``bleak`` lazily, inside the function that opens a
connection, so the protocol half imports and tests fine under a bare
interpreter. Its own ``lcd_push`` import is stdlib-only at module scope.
"""
import importlib.util
import pathlib
import sys

_SCRIPT = pathlib.Path(__file__).resolve().parents[2] / "scripts" / "vmu_read.py"
_spec = importlib.util.spec_from_file_location("vmu_read", _SCRIPT)
vmu_read = importlib.util.module_from_spec(_spec)
sys.modules["vmu_read"] = vmu_read
_spec.loader.exec_module(vmu_read)
