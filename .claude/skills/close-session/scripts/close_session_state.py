"""State block for /close-session, safe on any checkout.

The skill runs this script as its step 0. The real collector lives in the
private nested repo at `.dev/tools/close_session_state.py`; a public clone
has no `.dev`, so this wrapper prints a skip line and exits 0 there.

Exit codes:
  0   no `.dev` (skip line printed), or the delegate ran and exited 0.
  n   the delegate exited n != 0 (crash, interpreter error, killed): the
      last line is `wrapper: delegate exit n` and this wrapper exits n
      (1 if n is outside 0-255). State is incomplete; the caller must stop.
  2   unexpected wrapper error: `wrapper: error <ExcType>: <message>`.

The delegate itself exits 0 by its own contract and reports per-item
failures as `exit:` lines, so a non-zero exit here means it could not even
run to completion.
"""

import os
import subprocess
import sys

SKIP_LINE = "state: skipped (no .dev checkout — /close-session is an owner-only command)"


def _out(text):
    # Bytes, not the text layer: under redirection Windows Python defaults
    # stdout to CP932, which has no U+2014 and would die mid-write. UTF-8
    # keeps SKIP_LINE byte-exact; backslashreplace covers lone surrogates.
    data = text.encode("utf-8", errors="backslashreplace")
    sys.stdout.buffer.write(data)
    sys.stdout.flush()


def main():
    # .claude/skills/close-session/scripts/ -> repo root is four levels up.
    here = os.path.dirname(os.path.abspath(__file__))
    root = os.path.abspath(os.path.join(here, "..", "..", "..", ".."))
    delegate = os.path.join(root, ".dev", "tools", "close_session_state.py")
    if not os.path.isfile(delegate):
        _out(SKIP_LINE + "\n")
        return 0
    sys.stdout.flush()
    rc = subprocess.call([sys.executable, delegate] + sys.argv[1:], cwd=root)
    if rc != 0:
        _out("wrapper: delegate exit %d\n" % rc)
        return rc if 0 < rc <= 255 else 1
    return 0


if __name__ == "__main__":
    try:
        code = main()
    except SystemExit:
        raise
    except BaseException as exc:  # noqa: BLE001 - report, then fail loudly
        try:
            msg = str(exc).encode("ascii", errors="backslashreplace").decode("ascii")
            _out("wrapper: error %s: %s\n" % (type(exc).__name__, msg))
        except BaseException:
            pass
        code = 2
    sys.exit(code)
