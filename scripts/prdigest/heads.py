"""File heads and .gitattributes at a PR's base, for the measurement harness.

Production reads a changed file's first MARK_LINES lines and .gitattributes
from the checkout (pr_digest::resolve, file_head). The harness feeds each
fixture's base instead: for every PR, a folder STEM/ holding .gitattributes
(whole) and, for every modified, renamed or deleted file of any of its round
diffs, the file's first MARK_LINES lines at baseRefOid, at the path the
production route would read (the new path; the old one for a deletion).
Everything comes from a blobless clone (`git clone --filter=blob:none`,
`git show BASE:PATH`): no gh API call.

Usage:
  py -I heads.py set FIXTURE_DIR CLONES OUT_DIR   (bases from FIXTURE_DIR/manifest.json)
  py -I heads.py dev FIXTURE_DIR CLONES OUT_DIR   (bases from DEV_BASES below)
Writes OUT_DIR/STEM/... and OUT_DIR/SHA256SUMS (sha256 of every head file).
"""

import hashlib
import os
import re
import subprocess
import sys

MARK_LINES = 12
CLONE = {"rg": "ripgrep", "ruff": "ruff", "gh": "cli"}
# The dev set (prdigest/measure): baseRefOid from the pulls listings of the
# PRDIGEST lane, and one read-only `gh pr view 27081 --json baseRefOid`.
DEV_BASES = {
    "rg3452": "dfe4a81d2591daca76d25ae4e052c34b26578155",
    "rg3472": "fc3dd04b7ca18ec8be97704dc098bbd062bad244",
    "rg3483": "d958105d412b6a1ad014cb0e5832a9b029fa4977",
    "rg3487": "59e318f5ace48db54f37bb67c152535bc17fa153",
    "rg3560": "6892bc2b7efe5909cbbeec8f757ed2c4c76b1d30",
    "ruff27081": "ce0fb553538a01d09c0f9a135d68753d05118bfd",
}


def unquote(p):
    p = p.strip()
    if p.startswith('"') and p.endswith('"'):
        p = p[1:-1].encode("latin-1", "backslashreplace").decode("unicode_escape").encode("latin-1").decode("utf-8", "replace")
    return p


def changed_files(diff):
    """(read path, base path) of every file that exists at the base."""
    out = []
    for block in re.split(r"(?m)^(?=diff --git )", diff):
        if not block.startswith("diff --git "):
            continue
        head = block.split("\n@@ ", 1)[0]
        if re.search(r"(?m)^new file mode", head):
            continue
        old = new = None
        for line in head.split("\n"):
            if line.startswith("--- "):
                p = unquote(line[4:])
                old = None if p == "/dev/null" else p[2:]
            elif line.startswith("+++ "):
                p = unquote(line[4:])
                new = None if p == "/dev/null" else p[2:]
            elif line.startswith("rename from "):
                old = unquote(line[12:])
            elif line.startswith("rename to "):
                new = unquote(line[10:])
        if old is None:
            continue  # no hunks (binary, mode only): nothing a head check sees
        out.append((new or old, old))
    return out


def git_show(clone, spec):
    r = subprocess.run(["git", "-C", clone, "show", spec], capture_output=True)
    return r.stdout if r.returncode == 0 else None


def first_lines(data, n):
    out = b""
    for i, line in enumerate(data.splitlines(keepends=True)):
        if i == n:
            break
        out += line
    return out


def write_heads(fix, stem, base, clones, out):
    clone = os.path.join(clones, CLONE[re.match(r"[a-z]+", stem).group(0)])
    top = os.path.join(out, stem)
    os.makedirs(top, exist_ok=True)
    attrs = git_show(clone, f"{base}:.gitattributes")
    if attrs is not None:
        with open(os.path.join(top, ".gitattributes"), "wb") as f:
            f.write(attrs)
    files = {}
    for name in sorted(os.listdir(fix)):
        if name == f"{stem}.diff" or (name.startswith(f"{stem}.r") and name.endswith(".diff")):
            with open(os.path.join(fix, name), "rb") as f:
                for read, old in changed_files(f.read().decode("utf-8", "replace")):
                    files[read] = old
    missing = 0
    for read, old in sorted(files.items()):
        if ".." in read.split("/") or read.startswith("/"):
            continue
        data = git_show(clone, f"{base}:{old}")
        if data is None:
            missing += 1
            continue
        p = os.path.join(top, *read.split("/"))
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "wb") as f:
            f.write(first_lines(data, MARK_LINES))
    return len(files), missing, attrs is not None


def sums(out):
    lines = []
    for root, _, names in os.walk(out):
        for n in names:
            p = os.path.join(root, n)
            rel = os.path.relpath(p, out).replace(os.sep, "/")
            if rel == "SHA256SUMS":
                continue
            with open(p, "rb") as f:
                lines.append(f"{hashlib.sha256(f.read()).hexdigest()}  {rel}\n")
    lines.sort(key=lambda l: l[66:])
    with open(os.path.join(out, "SHA256SUMS"), "w", newline="\n") as f:
        f.writelines(lines)
    return hashlib.sha256("".join(lines).encode()).hexdigest()


def main():
    mode, fix, clones, out = sys.argv[1:5]
    if mode == "dev":
        bases = DEV_BASES
    else:
        import json
        with open(os.path.join(fix, "manifest.json"), encoding="utf-8") as f:
            bases = {p["name"]: p["base"] for p in json.load(f)["picks"]}
    for stem, base in sorted(bases.items()):
        n, missing, attrs = write_heads(fix, stem, base, clones, out)
        print(f"{stem}: {n} files, {missing} not at base, .gitattributes {'yes' if attrs else 'no'}")
    print(f"heads sha256 {sums(out)}")


if __name__ == "__main__":
    main()
