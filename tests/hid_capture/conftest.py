# SPDX-License-Identifier: GPL-3.0-or-later
"""Make ``hid_capture`` importable regardless of pytest's invocation directory.

The script imports ``hid`` lazily, inside the functions that open a device, so
the decoder half imports and tests fine under a bare interpreter.
"""
import importlib.util
import pathlib
import sys

_SCRIPT = pathlib.Path(__file__).resolve().parents[2] / "scripts" / "hid_capture.py"
_spec = importlib.util.spec_from_file_location("hid_capture", _SCRIPT)
hid_capture = importlib.util.module_from_spec(_spec)
sys.modules["hid_capture"] = hid_capture
_spec.loader.exec_module(hid_capture)
