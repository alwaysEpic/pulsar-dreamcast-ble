# SPDX-License-Identifier: GPL-3.0-or-later
"""Make ``hidlayout`` importable regardless of pytest's invocation directory."""
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
