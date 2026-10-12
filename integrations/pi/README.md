# pi extension

`fsearch.ts` replaces the `grep` tool in pi (`@earendil-works/pi-coding-agent`).
A search over a folder outside any git repo, inside `FSEARCH_ROOTS` and
`FSEARCH_HOME`, goes to the fsearch daemon. Everything else (a repo path, a
glob, context lines, an index still building) runs pi's built-in ripgrep grep.

Install it by copying the file to `~/.pi/agent/extensions/`. `fsearch` must be
on PATH.
