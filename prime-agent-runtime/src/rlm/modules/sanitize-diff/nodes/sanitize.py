"""Clean a unified diff before an agent reads it."""
import re

SECRET = re.compile(r"(?i)(api[_-]?key|secret|token|password|private[_-]?key)\s*[:=]")
CAP = 200_000


def sanitize(inputs):
    out, skip = [], False
    for line in (inputs.get("diff") or "").splitlines():
        if line.startswith("diff --git"):
            skip = False
        if line.startswith("Binary files ") or line.startswith("GIT binary patch"):
            skip = True
            out.append("[binary change omitted]")
            continue
        if skip:
            continue
        if line[:1] in "+-" and SECRET.search(line):
            out.append(line[0] + "[line with a secret-like value omitted]")
            continue
        out.append(line)
    text = "\n".join(out)
    if len(text) > CAP:
        text = text[:CAP] + "\n[diff truncated]"
    return {"diff": text}
