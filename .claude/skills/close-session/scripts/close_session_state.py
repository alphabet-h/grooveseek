"""State block for /close-session, safe on any checkout.

The skill runs this script as its step 0. The real collector lives in the
private nested repo at `.dev/tools/close_session_state.py`; a public clone
has no `.dev` directory, so this wrapper prints a skip line and exits 0 there.
A `.dev` that exists without the delegate is a broken owner setup (for
example an older `.dev` revision), not a public clone: that is an error.

Exit codes:
  0   no `.dev` directory (skip line printed), or the delegate ran and
      exited 0.
  2   `.dev` exists but `.dev/tools/close_session_state.py` does not: the
      last line is `wrapper: error delegate missing: <path> (update the
      .dev checkout)`. The caller must stop.
  2   `.dev` and the delegate exist but `.dev` is not its own git repository
      (`git -C .dev rev-parse --show-toplevel` fails or does not end in
      `/.dev`, the same check /next-work makes): the last line is `wrapper:
      error .dev is not a nested git repository: <toplevel or git error>`.
      The delegate is not run. The caller must stop.
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


def _decode(data):
    try:
        return data.decode("utf-8")
    except UnicodeDecodeError:
        return data.decode("cp932", errors="replace")


def _one_line(text):
    return " ".join(text.split()) or "(no output)"


def _nested_repo_problem(dev):
    """None when `dev` is its own git repo (toplevel ends in /.dev), else a one-line reason.

    Same predicate as /next-work's precondition: `rev-parse --show-toplevel`
    must end in `/.dev`. A `.dev` copied without its `.git` resolves to the
    outer repo and fails here.
    """
    try:
        proc = subprocess.run(
            ["git", "-C", dev, "rev-parse", "--show-toplevel"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=15,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        return "git failed: %s" % _one_line(str(exc))
    if proc.returncode != 0:
        return "git failed: %s" % _one_line(_decode(proc.stderr))
    top = _decode(proc.stdout).strip().replace("\\", "/")
    if not top.lower().endswith("/.dev"):
        return _one_line(top)
    return None


def main():
    # .claude/skills/close-session/scripts/ -> repo root is four levels up.
    here = os.path.dirname(os.path.abspath(__file__))
    root = os.path.abspath(os.path.join(here, "..", "..", "..", ".."))
    dev = os.path.join(root, ".dev")
    delegate = os.path.join(dev, "tools", "close_session_state.py")
    if not os.path.isdir(dev):
        _out(SKIP_LINE + "\n")
        return 0
    if not os.path.isfile(delegate):
        _out("wrapper: error delegate missing: %s (update the .dev checkout)\n" % delegate)
        return 2
    problem = _nested_repo_problem(dev)
    if problem is not None:
        _out("wrapper: error .dev is not a nested git repository: %s\n" % problem)
        return 2
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
