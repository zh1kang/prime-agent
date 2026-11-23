"""Run the project's checks in the run's working directory."""
import os
import re
import subprocess

LOG_CHARS = 8000
TAIL_LINES = 80
ERROR_LINES = 20
ERROR_LINE = re.compile(r"error|fail|panic|exception|traceback|assert", re.IGNORECASE)


def _flaky_retries(inputs):
    """Re-runs of a failed check: the flaky_retries input, else the
    PA_VALIDATE_FLAKY_RETRIES env var, else 0 (off)."""
    for raw in (inputs.get("flaky_retries"), os.environ.get("PA_VALIDATE_FLAKY_RETRIES")):
        if raw is None or raw == "":
            continue
        try:
            return max(0, int(raw))
        except (TypeError, ValueError):
            continue
    return 0


def _failure_log(text):
    """A failed run's output cut to what a fixer reads: the error-looking
    lines above the tail, then the last TAIL_LINES lines."""
    lines = text.splitlines()
    if len(lines) <= TAIL_LINES:
        return text[-LOG_CHARS:]
    head, tail = lines[:-TAIL_LINES], lines[-TAIL_LINES:]
    errors = [line for line in head if ERROR_LINE.search(line)][-ERROR_LINES:]
    parts = []
    if errors:
        parts += ["[earlier error lines]", *errors]
    parts += [f"[last {TAIL_LINES} of {len(lines)} lines]", *tail]
    return "\n".join(parts)[-LOG_CHARS:]


def _check(command):
    done = subprocess.run(command, shell=True, capture_output=True, text=True, stdin=subprocess.DEVNULL)
    return done.returncode, done.stdout + done.stderr


def run(inputs):
    command = (inputs.get("validate_command") or "").strip()
    if not command:
        raise ValueError("Set the validate_command param to the command that runs the checks.")
    code, output = _check(command)
    if code == 0:
        return {"passed": True, "log": f"$ {command}\nexit 0\n{output[-LOG_CHARS:]}"}
    rerun_command = (inputs.get("rerun_failed_cmd") or "").strip()
    retries = _flaky_retries(inputs) or (1 if rerun_command else 0)
    for attempt in range(1, retries + 1):
        rerun_code, rerun_output = _check(rerun_command or command)
        if rerun_code == 0:
            return {
                "passed": True,
                "log": (
                    f"$ {command}\nflaky: exit {code} first, then passed on re-run {attempt}"
                    f"{f' ($ {rerun_command})' if rerun_command else ''}\n"
                    f"--- first run ---\n{_failure_log(output)}\n"
                    f"--- passing re-run ---\n{rerun_output[-LOG_CHARS // 2:]}"
                ),
            }
        code, output = rerun_code, rerun_output
    return {"passed": False, "log": f"$ {command}\nexit {code}\n{_failure_log(output)}"}
