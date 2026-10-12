"""Do fsearch and ripgrep agree once ripgrep sees what fsearch indexes?

ripgrep runs with -uu (no ignore files, hidden included) and its file list is
filtered by fsearch's own eligibility rules (content.rs `eligible`). What is
left over on either side is a real disagreement, not a scope difference.

usage: python bench/parity.py <root> <out.json>
"""

import json
import os
import re
import subprocess
import sys

sys.path.insert(0, os.path.dirname(__file__))
from win_vs_rg import QUERIES, RG, fsearch  # noqa: E402

SRC = open(os.path.join(os.path.dirname(__file__), "..", "src", "content.rs"), encoding="utf-8").read()


def byte_list(name):
    body = re.search(rf"const {name}: &\[&\[u8\]\] = &\[(.*?)\];", SRC, re.S).group(1)
    return re.findall(r'b"([^"]*)"', body)


SKIP_DIRS = set(byte_list("SKIP_DIRS"))
SKIP_SUFFIXES = byte_list("SKIP_SUFFIXES")
TEXT_EXTS = {x.lower() for x in byte_list("TEXT_EXTS")}


def eligible(root, rel):
    parts = rel.split("/")
    if any(c in SKIP_DIRS or any(len(c) > len(x) and c.endswith(x) for x in SKIP_SUFFIXES) for c in parts[:-1]):
        return False
    try:
        size = os.stat(os.path.join(root, rel)).st_size
    except OSError:
        return False
    name = parts[-1]
    if size > 1 << 20:
        return False
    dot = name.rfind(".")
    if dot > 0:
        return name[dot + 1:].lower() in TEXT_EXTS and not name.endswith(".min.js") and name != "package-lock.json"
    return size <= 256 << 10


def main():
    root, out = sys.argv[1], sys.argv[2]
    report = {}
    for label, fq, pat in QUERIES:
        _, _, fs, _ = fsearch(root, fq)
        p = subprocess.run([RG, "-uu", "-S", "-l", "--max-filesize", "1M", "--", pat], cwd=root, capture_output=True)
        raw = [line.replace("\\", "/") for line in p.stdout.decode("utf-8", "replace").splitlines()]
        rg = {r.lower() for r in raw if eligible(root, r)}
        only_fs, only_rg = sorted(fs - rg), sorted(rg - fs)
        report[label] = {"fsearch": len(fs), "rg_uu_raw": len(raw), "rg_uu_eligible": len(rg), "both": len(fs & rg),
                         "only_fsearch_n": len(only_fs), "only_rg_n": len(only_rg),
                         "only_fsearch": only_fs[:20], "only_rg": only_rg[:20]}
        print(label, {k: v for k, v in report[label].items() if not k.startswith("only_") or k.endswith("_n")}, flush=True)
    json.dump(report, open(out, "w"), indent=1)


if __name__ == "__main__":
    main()
