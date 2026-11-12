"""A virtual clock runner for the script tests.

Time only moves when the script under test sleeps, so a timeout scenario
cannot race host load. `subprocess` is imported first, before time is
patched, so it keeps the real monotonic clock it bound at import for its
own subprocess timeouts.
"""

from pathlib import Path

PRELUDE = """import subprocess
import runpy
import sys
import time

now = [0.0]
time.monotonic = lambda: now[0]
time.sleep = lambda seconds: now.__setitem__(0, now[0] + seconds)
"""

VIRTUAL_CLOCK_RUNNER = (
    "\n"
    + PRELUDE
    + """sys.argv = sys.argv[1:]
runpy.run_path(sys.argv[0], run_name="__main__")
"""
)


def wrapper_for(target: Path) -> str:
    """Source of a Python script that runs `target` under the virtual clock with this script's arguments."""
    return (
        "#!/usr/bin/env python3\n"
        + PRELUDE
        + f"sys.argv[0] = {str(target)!r}\n"
        + f"runpy.run_path({str(target)!r}, run_name=\"__main__\")\n"
    )
