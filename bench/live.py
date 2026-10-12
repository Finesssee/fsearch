"""End-to-end check of the Windows daemon: auto-start over the pipe, live
name and content updates, catch-up after a restart, and idle cost.

Run with FSEARCH_ROOTS and FSEARCH_HOME set the way the index was built.
Test files are created under bench/ and removed with ~/.claude/bin/trash."""

import json
import os
import statistics
import subprocess
import sys
import time
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
FSEARCH = os.path.join(HERE, "..", "target", "release", "fsearch.exe")
TRASH = os.path.expanduser("~/.claude/bin/trash")
BASH = r"C:\Program Files\Git\bin\bash.exe"


def ask(q):
    p = subprocess.run([FSEARCH, q, "--json"], capture_output=True, text=True, timeout=60)
    return json.loads(p.stdout or "{}")


def paths(v):
    return {p["path"].lstrip("/") for p in v.get("hits", []) + v.get("files", [])}


def wait_for(q, want, present=True, limit=30.0):
    t = time.perf_counter()
    while time.perf_counter() - t < limit:
        if (want in paths(ask(q))) == present:
            return time.perf_counter() - t
        time.sleep(0.01)
    return None


def trash(path):
    subprocess.run([BASH, TRASH, path], check=True, capture_output=True)


def daemon_pid():
    out = subprocess.run(["tasklist", "/FI", "IMAGENAME eq fsearch.exe", "/FO", "CSV", "/NH"], capture_output=True, text=True).stdout
    pids = [int(l.split('","')[1]) for l in out.splitlines() if l.startswith('"fsearch.exe"')]
    return pids[0] if pids else None


def idle_cost(pid, secs=30):
    ps = f"$p=Get-Process -Id {pid}; $c=$p.TotalProcessorTime.TotalMilliseconds; Start-Sleep {secs}; $p.Refresh(); " \
         f"'{{0}} {{1}}' -f ($p.TotalProcessorTime.TotalMilliseconds-$c), $p.WorkingSet64"
    cpu_ms, ws = subprocess.run(["pwsh", "-NoProfile", "-Command", ps], capture_output=True, text=True).stdout.split()
    return float(cpu_ms) / (secs * 1000) * 100, int(ws) / 1e6


def to_index(p):
    return p.replace("\\", "/")


def main():
    t = time.perf_counter()
    p = subprocess.run(f'"{FSEARCH}" status | findstr ok', shell=True, capture_output=True, text=True, timeout=120)
    print(f"cold start over pipe, piped through findstr: {time.perf_counter() - t:.2f} s, returned={p.returncode == 0}")

    rounds = []
    for i in range(5):
        tok = "zq" + uuid.uuid4().hex[:12]
        f = os.path.join(HERE, f"live_{tok}.txt")
        ip = to_index(f)
        with open(f, "w") as h:
            h.write(f"first line\n{tok}_alpha\n")
        create_name = wait_for(f"live_{tok}", ip)
        create_body = wait_for(f"grep:{tok}_alpha", ip)
        with open(f, "w") as h:
            h.write(f"first line\n{tok}_beta\n")
        edit_body = wait_for(f"grep:{tok}_beta", ip)
        edit_gone = wait_for(f"grep:{tok}_alpha", ip, present=False)
        trash(f)
        del_name = wait_for(f"live_{tok}", ip, present=False)
        del_body = wait_for(f"grep:{tok}", ip, present=False)
        rounds.append((create_name, create_body, edit_body, edit_gone, del_name, del_body))
        print(f"round {i}: " + " ".join("miss" if x is None else f"{x * 1000:.0f}ms" for x in rounds[-1]))
    for name, col in zip(["create name", "create content", "edit new text", "edit old text gone", "delete name", "delete content"], zip(*rounds)):
        ok = [x for x in col if x is not None]
        print(f"  {name:20} median {statistics.median(ok) * 1000:6.0f} ms  found {len(ok)}/5" if ok else f"  {name:20} missed 5/5")

    pid = daemon_pid()
    cpu, ws = idle_cost(pid)
    print(f"idle daemon pid {pid}: {cpu:.2f}% of one core over 30 s, working set {ws:.0f} MB")

    subprocess.run([FSEARCH, "stop"], check=True)
    tok = "zq" + uuid.uuid4().hex[:12]
    f = os.path.join(HERE, f"offline_{tok}.txt")
    with open(f, "w") as h:
        h.write(f"{tok}_offline\n")
    t = time.perf_counter()
    name = wait_for(f"offline_{tok}", to_index(f))
    body = wait_for(f"grep:{tok}_offline", to_index(f))
    print(f"made while stopped, found after restart: name {name and round(name, 2)} s, content {body and round(body, 2)} s (from restart)")
    trash(f)


if __name__ == "__main__":
    sys.exit(main())
