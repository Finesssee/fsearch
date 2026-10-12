"""Catch-up after a restart: change files while the daemon is stopped, start
it again and time how long until every change is searchable.

Run once normally and once from an elevated shell; only the elevated daemon
can read the change journal. Set FSEARCH_ROOTS and FSEARCH_HOME the way the
index was built. Test files live under bench/catchup_*/ and are removed with
~/.claude/bin/trash."""

import json
import os
import subprocess
import sys
import time
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
FSEARCH = os.environ.get("FSEARCH_BIN") or os.path.join(HERE, "..", "target", "release", "fsearch.exe")
TRASH = os.path.expanduser("~/.claude/bin/trash")
BASH = r"C:\Program Files\Git\bin\bash.exe"
LOG = os.path.join(os.environ["LOCALAPPDATA"], "FSearch", "daemon.log")


def ask(req):
    p = subprocess.run([FSEARCH, "stdio"], input=(json.dumps(req) + "\n").encode(), capture_output=True, timeout=60)
    return json.loads(p.stdout or b"{}")


def found(q, want):
    v = ask({"q": q, "budget_ms": 0})
    return want in {p["path"].lstrip("/") for p in v.get("hits", []) + v.get("files", [])}


def wait(pred, limit=60.0):
    t = time.perf_counter()
    while time.perf_counter() - t < limit:
        if pred():
            return time.perf_counter() - t
        time.sleep(0.02)
    return None


def ready():
    return ask({"op": "status"}).get("ok") is True


def main():
    subprocess.run([FSEARCH, "stop"], check=True)
    log_from = os.path.getsize(LOG) if os.path.exists(LOG) else 0
    wait(ready)
    # The journal position is noted at start and every 30 s; the index must
    # be saved more than 10 s after a note for the journal to be usable.
    keep = os.path.join(HERE, f"catchup_keep_{uuid.uuid4().hex[:8]}.txt")
    with open(keep, "w") as h:
        h.write("before\n")
    gone = os.path.join(HERE, f"catchup_gone_{uuid.uuid4().hex[:8]}.txt")
    with open(gone, "w") as h:
        h.write("x\n")
    time.sleep(15)
    with open(keep, "a") as h:
        h.write("bump\n")
    time.sleep(3)
    ask({"op": "save"})
    time.sleep(5)
    subprocess.run([FSEARCH, "stop"], check=True)

    tok = "zq" + uuid.uuid4().hex[:10]
    d = os.path.join(HERE, f"catchup_{tok}", "nested")
    os.makedirs(d)
    new = os.path.join(d, f"new_{tok}.txt")
    with open(new, "w") as h:
        h.write(f"{tok}_created\n")
    # An edit changes the file, not its folder's mtime.
    with open(keep, "w") as h:
        h.write(f"{tok}_edited\n")
    subprocess.run([BASH, TRASH, gone], check=True, capture_output=True)

    t = time.perf_counter()
    wait(ready)
    up = time.perf_counter() - t
    checks = {
        "new file in new folder (name)": lambda: found(f"new_{tok}", new.replace("\\", "/")),
        "new file (content)": lambda: found(f"grep:{tok}_created", new.replace("\\", "/")),
        "edited file (content)": lambda: found(f"grep:{tok}_edited", keep.replace("\\", "/")),
        "deleted file (name)": lambda: not found(os.path.basename(gone)[:-4], gone.replace("\\", "/")),
    }
    print(f"daemon ready {up:.2f} s after restart")
    for name, pred in checks.items():
        r = wait(pred)
        print(f"  {name:32} {'missed after 60 s' if r is None else f'{time.perf_counter() - t:.2f} s after restart'}")
    with open(LOG, encoding="utf-8", errors="replace") as h:
        h.seek(log_from)
        for line in h.read().splitlines():
            if "journal" in line or "relisted" in line or "content:" in line:
                print("  log:", line)
    subprocess.run([FSEARCH, "stop"], check=True)
    for p in [os.path.join(HERE, f"catchup_{tok}"), keep]:
        subprocess.run([BASH, TRASH, p], capture_output=True)


if __name__ == "__main__":
    sys.exit(main())
