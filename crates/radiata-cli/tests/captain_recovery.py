"""Exercise the installed CLI boundary with isolated projects and scripted providers."""

import atexit, json, os, pathlib, sqlite3, subprocess, sys, time
from concurrent.futures import ThreadPoolExecutor

P = pathlib.Path
binary = str(P(sys.argv[1]).resolve())
temp = P(sys.argv[2]).resolve()
a = temp / "alpha"
b = temp / "beta"
a.mkdir()
b.mkdir()
hub = temp / "hub"
native = temp / "native"
native.mkdir()
mock = temp / "bin"
mock.mkdir()
script = mock / "claude"
script.write_text("""#!/usr/bin/env python3
import json,sys,os,pathlib
body=sys.stdin.read()
capture=pathlib.Path(os.environ["CAPTURE"])
with capture.open("a") as f:f.write(json.dumps({"cwd":os.getcwd(),"args":sys.argv[1:],"body":body})+"\\n")
if "patch test" in body:pathlib.Path("crew.txt").write_text("tested")
if "crash-check" in body:
 import time
 for _ in range(1000):
  if (capture.parent/"release").exists():break
  time.sleep(.03)
print(json.dumps({"type":"result","session_id":"fa16e11d-0a87-47ae-a8d1-f6e3a12b806f","result":"done in linked checkout","is_error":False}))
""")
script.chmod(0o755)
# Prime turns run on this daemon; the macOS socket path limit is 104 bytes.
SOCK = f"/tmp/pa-crc-{os.getpid()}.sock"
atexit.register(subprocess.run, ["pkill", "-f", SOCK], check=False)
env = dict(
    os.environ,
    PA_CAPTAIN_HOME=str(hub),
    PRIME_AGENT_CODING_AGENT_DIR=str(hub),
    PA_CAPTAIN_NATIVE_HOME=str(native),
    PRIME_AGENT_DAEMON_SOCKET=str(temp / "socket"),
    PA_CAPTAIN_NATIVE_SOCKET=SOCK,
    PRIME_AGENT_FAUX_SCRIPT=json.dumps({"responses": ["Captain received the report."]}),
    DO_NOT_TRACK="1",
    CAPTURE=str(temp / "capture"),
    PATH=str(mock) + ":" + os.environ["PATH"],
)


def run(*args, cwd=a, body=None, allow=False):
    cp = subprocess.run(
        [binary, "thread", *args],
        cwd=cwd,
        env=env,
        input=body,
        text=True,
        capture_output=True,
        timeout=30,
    )
    if not allow and cp.returncode:
        raise RuntimeError(str(args) + " " + cp.stderr[-750:])
    return cp


def act(op, cwd=a, **args):
    return json.loads(run("captain", "act", op, cwd=cwd, body=json.dumps(args)).stdout)


def stat(cwd=a):
    return json.loads(run("captain", "status", cwd=cwd).stdout)


def git(cwd, *args):
    return subprocess.check_output(["git", "-C", str(cwd), *args], text=True)


def db():
    return sqlite3.connect(hub / "captains/projects.sqlite3")


def wait(f):
    for i in range(250):
        res = f()
        if res:
            return res
        time.sleep(0.15)
    raise RuntimeError(
        "timeout "
        + str(db().execute("select purpose,status,result from jobs").fetchall())
    )


try:
    git(a, "init", "-q")
    p = act("init", objective="Ship Alpha")
    assert act("init", objective="New title")["id"] == p["id"]
    q = act("init", cwd=b, objective="Ship Beta")
    assert stat()["current"] == p["id"] and stat(b)["current"] == q["id"]
    nohead = run(
        "captain",
        "act",
        "delegate",
        body=json.dumps(
            {"project": p["id"], "task": "patch", "provider": "claude", "write": True}
        ),
        allow=True,
    )
    assert nohead.returncode and "commit" in nohead.stderr, nohead
    git(
        a,
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=test@localhost",
        "commit",
        "--allow-empty",
        "-qm",
        "base",
    )

    def delegate(_):
        return act(
            "delegate",
            project=p["id"],
            provider="claude",
            task="patch test",
            write=True,
            request_id="op-1",
        )

    with ThreadPoolExecutor(2) as pool:
        w1, w2 = pool.map(delegate, range(2))
    assert w1 == w2, (w1, w2)
    wait(
        lambda: (
            db()
            .execute(
                "select count(*) from jobs where purpose='digest' and status='done'"
            )
            .fetchone()[0]
        )
    )
    row = stat()["projects"][0]["crew"][0]
    assert row["id"] == w1["id"] and row["state"] == "done", row
    tree = P(row["cwd"])
    assert (tree / "crew.txt").is_file() and not (a / "crew.txt").exists()
    assert stat(tree)["current"] == p["id"]
    assert len((temp / "capture").read_text().splitlines()) == 1
    tid = run("new", "claude").stdout.strip()
    act("adopt", project=p["id"], ref=tid)
    assert run(
        "captain",
        "act",
        "adopt",
        cwd=b,
        body=json.dumps({"project": q["id"], "ref": tid}),
        allow=True,
    ).returncode
    run(tid, "send-stdin", body="external turn")
    wait(
        lambda: (
            db().execute("select detail from crew where id=?", (tid,)).fetchone()[0]
            == "done in linked checkout"
        )
    )
    act("release", project=p["id"], member=tid)
    assert run(tid, "context").returncode == 0
    # Kill only the isolated Captain wrapper while its provider child is still active.
    crash = act(
        "delegate",
        project=p["id"],
        provider="claude",
        task="crash-check",
        write=False,
        request_id="crash-check",
    )
    wait(lambda: "crash-check" in (temp / "capture").read_text())
    import signal

    worker = (
        db().execute("select pid from jobs where id=?", (crash["job"],)).fetchone()[0]
    )
    os.kill(worker, signal.SIGKILL)
    wait(
        lambda: (
            stat()
            and db()
            .execute("select status from jobs where id=?", (crash["job"],))
            .fetchone()[0]
            == "interrupted"
        )
    )
    (temp / "release").touch()
    wait(
        lambda: (
            stat()
            and db()
            .execute("select status from jobs where id=?", (crash["job"],))
            .fetchone()[0]
            == "done"
        )
    )
    assert (
        act(
            "delegate",
            project=p["id"],
            provider="claude",
            task="crash-check",
            write=False,
            request_id="crash-check",
        )
        == crash
    )
    entries = [json.loads(line) for line in (temp / "capture").read_text().splitlines()]
    assert sum("crash-check" in entry["body"] for entry in entries) == 1
    assert (
        db()
        .execute("select count(*) from reports where id=?", ("job/" + crash["job"],))
        .fetchone()[0]
        == 1
    )
    print(
        "Verified a killed dispatcher, delayed provider completion, and exactly one worker and completion report."
    )
    print(
        "Verified two projects, unborn repo, concurrent idempotent dispatch, linked writer, automatic Captain wake, adopted thread reports and release.",
        flush=True,
    )
finally:
    # Drop watch subscriptions in this isolated test store so detached watchers exit.
    if (hub / "captains/projects.sqlite3").is_file():
        with db() as c:
            c.execute("delete from watches")
