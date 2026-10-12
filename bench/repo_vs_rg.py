"""fsearch daemon vs ripgrep inside one repo, end to end per process.

fsearch runs through the installed daemon (whole-D index) with `in:<repo>`;
ripgrep runs the way an agent's Grep does (`rg -l` in the repo, .gitignore
respected) and once more over fsearch's file set for a parity diff. Sides run
interleaved per round, order flipped on odd rounds.

usage: python bench/repo_vs_rg.py <repo> <rounds> <out.json>
"""

import json
import os
import statistics
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(__file__))
from win_vs_rg import scoped_globs  # noqa: E402

FSEARCH = os.environ.get("FSEARCH_BIN", "fsearch")
RG = os.environ.get("RG", "rg")
ENV = dict(os.environ, FSEARCH_ROOTS="D:", FSEARCH_HOME="D:/")

QUERIES = [
    ("rare literal", "grep:ggml_backend_sched_reserve", "ggml_backend_sched_reserve"),
    ("common phrase", 'grep:"int main"', "int main"),
    ("regex", r"regex:ggml_\w+_f16", r"ggml_\w+_f16"),
    ("common identifier", "grep:GGML_ASSERT", "GGML_ASSERT"),
    ("very common word", "grep:include", "include"),
]


def fsearch(repo, q):
    scope = "/" + repo.replace("\\", "/").rstrip("/")
    req = json.dumps({"q": f"{q} in:{scope}", "limit": 10000000, "budget_ms": 0, "per_file": 1}) + "\n"
    t = time.perf_counter()
    p = subprocess.run([FSEARCH, "stdio"], input=req.encode(), capture_output=True, env=ENV)
    dt = time.perf_counter() - t
    v = json.loads(p.stdout or b"{}")
    files = {f["path"].removeprefix(scope + "/").lower() for f in v.get("files", [])}
    return dt, files, {"took_us": v.get("took_us"), "complete": v.get("complete")}


def fsearch_page(repo, q):
    scope = "/" + repo.replace("\\", "/").rstrip("/")
    t = time.perf_counter()
    subprocess.run([FSEARCH, f"{q} in:{scope}"], capture_output=True, env=ENV)
    return time.perf_counter() - t, set(), {}


def rg(repo, pat, scoped):
    extra = ["--max-filesize", "1M", *scoped_globs()] if scoped else []
    t = time.perf_counter()
    p = subprocess.run([RG, "-S", "-l", *extra, "--", pat], capture_output=True, cwd=repo)
    dt = time.perf_counter() - t
    files = {line.replace("\\", "/").lower() for line in p.stdout.decode("utf-8", "replace").splitlines()}
    return dt, files, {"code": p.returncode}


def main():
    repo, rounds, out = sys.argv[1], int(sys.argv[2]), sys.argv[3]
    sides = [
        ("fsearch all matches", lambda q: fsearch(repo, q[1])),
        ("fsearch first page", lambda q: fsearch_page(repo, q[1])),
        ("rg default", lambda q: rg(repo, q[2], False)),
        ("rg on fsearch's files", lambda q: rg(repo, q[2], True)),
    ]
    runs = {q[0]: {s: [] for s, _ in sides} for q in QUERIES}
    last = {}
    for r in range(rounds):
        for q in QUERIES:
            for name, fn in sides if r % 2 == 0 else sides[::-1]:
                dt, files, info = fn(q)
                runs[q[0]][name].append({"s": dt, "n": len(files), **info})
                last[(q[0], name)] = files
                print(f"round {r} {q[0]:18} {name:22} {dt * 1000:9.1f} ms {len(files):6} files", flush=True)
    summary = {}
    for label, *_ in QUERIES:
        s = {name: {"median_ms": round(statistics.median(x["s"] for x in v) * 1000, 1), "files": v[-1]["n"]} for name, v in runs[label].items()}
        s["fsearch all matches"]["server_ms"] = round(statistics.median(x["took_us"] or 0 for x in runs[label]["fsearch all matches"]) / 1000, 2)
        fs = last[(label, "fsearch all matches")]
        for other in ("rg default", "rg on fsearch's files"):
            o = last[(label, other)]
            s[f"vs {other}"] = {"only_fsearch": len(fs - o), "only_rg": len(o - fs), "only_rg_sample": sorted(o - fs)[:8], "only_fsearch_sample": sorted(fs - o)[:8]}
        summary[label] = s
    json.dump({"runs": runs, "summary": summary}, open(out, "w"), indent=1)
    print(json.dumps(summary, indent=1))


if __name__ == "__main__":
    main()
