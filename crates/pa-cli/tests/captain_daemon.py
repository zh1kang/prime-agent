"""A minimal client of the Prime Agent daemon socket for the Captain e2e scripts."""
import json, os, socket, uuid

PROTOCOL = {"name": "prime-agent.daemon", "version": 7}


class Connection:
    """One socket connection: send commands and read the frames the daemon writes."""

    def __init__(self, path, timeout=120):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(timeout)
        self.sock.connect(path)
        self.buffer = b""
        self.events = []

    def frame(self):
        while b"\n" not in self.buffer:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise ConnectionError("the daemon closed the connection")
            self.buffer += chunk
        line, self.buffer = self.buffer.split(b"\n", 1)
        return json.loads(line)

    def request(self, command):
        rid = str(uuid.uuid4())
        self.sock.sendall((json.dumps({"type": "command", "id": rid, "protocol": PROTOCOL, "command": command}) + "\n").encode())
        while True:
            frame = self.frame()
            if frame.get("id") == rid and frame.get("type") == "response":
                return frame
            self.events.append(frame)

    def close(self):
        self.sock.close()


def request(path, command):
    connection = Connection(path)
    try:
        return connection.request(command)
    finally:
        connection.close()


def live_session(path, session_file):
    """The daemon's live session that serves `session_file`, or None."""
    try:
        listed = request(path, {"type": "list", "includeClientOwned": True})
    except (FileNotFoundError, ConnectionRefusedError):
        return None  # the daemon has not started yet
    target = os.path.realpath(session_file)
    for row in listed.get("data", {}).get("sessions", []):
        for key in ("sessionFile", "sessionPath", "path"):
            if row.get(key) and os.path.realpath(row[key]) == target:
                return row
    return None


def retire(path, session_file):
    """Stop the daemon worker that serves `session_file`, so the next turn starts a fresh one
    that reads the current faux script."""
    row = live_session(path, session_file)
    if row:
        request(path, {"type": "kill", "activeSessionId": row.get("activeSessionId") or row["id"]})
