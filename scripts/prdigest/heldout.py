"""Pick and fetch the PR digest held-out set (step 0).

The rule, fixed before any lever lands:

* Repos: BurntSushi/ripgrep, astral-sh/ruff (Rust) and cli/cli (Go, the
  non-Rust repo).
* Candidates: merged PRs merged before 2026-11-08T00:00Z, among the 100 most
  recently created merged PRs of each repo, newest merge first. Excluded: the
  dev and inspected PRs (every PR in prdigest/raw), and bot authors.
* Pre-filter: additions + deletions >= 150 (a cheaper stand-in that saves gh
  calls; the size rule below is what decides).
* Size: the final `gh pr diff` is at least 12 KB (12288 bytes).
* Rounds: each distinct reviews[].commit.oid, in order of its first review.
  A round's diff is `compare baseRefOid...oid`, or the final diff when the
  oid is the head. A PR with no review is one round, its head. At most three
  rounds are kept: the first two and the last. A round whose diff is larger
  than the final diff and names a file the final diff does not was diffed
  against another base (a stacked PR retargeted later, or a base merge); it
  is dropped.
* Picks: per repo the 4 most recent qualifying PRs; when fewer than 2 of them
  have 2+ rounds, the oldest one-round picks give way to the next most recent
  qualifying PRs with 2+ rounds. At most 9 diff fetches per repo.
* Each pick also keeps its `gh pr view --json` and its inline review comments
  (`pulls/N/comments`), for the view arm and the fidelity proxy.

Read-only gh calls only, counted against a hard budget. Every fetch is cached
in the output folder, so a rerun spends nothing. Writes manifest.json and
SHA256SUMS (sha256 of every fixture) to the output folder.

Usage: py -I heldout.py OUT_DIR [BUDGET]
"""

import hashlib
import json
import os
import re
import subprocess
import sys

REPOS = [("BurntSushi/ripgrep", "rg"), ("astral-sh/ruff", "ruff"), ("cli/cli", "gh")]
CUTOFF = "2026-11-08T00:00:00Z"
EXCLUDE = {
    "BurntSushi/ripgrep": {3452, 3472, 3483, 3487, 3560},
    "astral-sh/ruff": {27081, 30090, 30129},
    "cli/cli": set(),
}
MIN_BYTES = 12 * 1024
MIN_CHANGED = 150
PER_REPO = 4
MULTI_PER_REPO = 2
MAX_DIFFS_PER_REPO = 9
MAX_ROUNDS = 3
VIEW_FIELDS = "number,title,body,author,state,headRefOid,baseRefOid,comments,reviews,commits"


class Budget:
    def __init__(self, n):
        self.left = n
        self.used = 0

    def gh(self, args):
        if self.left <= 0:
            raise SystemExit(f"gh budget spent ({self.used} calls); stopping")
        self.left -= 1
        self.used += 1
        print("gh", " ".join(args), file=sys.stderr)
        r = subprocess.run(["gh", *args], capture_output=True)
        if r.returncode != 0:
            return None, r.stderr.decode("utf-8", "replace").strip()
        return r.stdout, ""


def cached(path, fetch):
    if os.path.exists(path):
        with open(path, "rb") as f:
            return f.read(), ""
    data, err = fetch()
    if data is not None:
        with open(path, "wb") as f:
            f.write(data)
    return data, err


def diff_files(data):
    return set(re.findall(rb"(?m)^diff --git a/(\S+) b/", data))


def rounds_of(reviews, head):
    seen = []
    for r in sorted(reviews or [], key=lambda r: r.get("submittedAt") or ""):
        oid = ((r.get("commit") or {}).get("oid")) or ""
        if oid and oid not in seen:
            seen.append(oid)
    if not seen:
        seen = [head]
    if len(seen) > MAX_ROUNDS:
        seen = seen[:2] + [seen[-1]]
    return seen


def main():
    out = sys.argv[1]
    budget = Budget(int(sys.argv[2]) if len(sys.argv) > 2 else 66)
    os.makedirs(out, exist_ok=True)
    manifest = {"rule": __doc__.split("Read-only")[0].strip(), "cutoff": CUTOFF, "repos": {}, "picks": []}
    for repo, tag in REPOS:
        lst, err = cached(
            os.path.join(out, f"{tag}.list.json"),
            lambda: budget.gh(["pr", "list", "-R", repo, "--state", "merged", "--limit", "100",
                               "--json", "number,mergedAt,additions,deletions,author,baseRefOid,headRefOid,reviews"]),
        )
        if lst is None:
            raise SystemExit(f"{repo} list: {err}")
        prs = json.loads(lst)
        cands = [
            p for p in prs
            if p.get("mergedAt") and p["mergedAt"] < CUTOFF
            and p["number"] not in EXCLUDE[repo]
            and not (p.get("author") or {}).get("is_bot")
            and not (p.get("author") or {}).get("login", "").endswith("[bot]")
            and p.get("additions", 0) + p.get("deletions", 0) >= MIN_CHANGED
        ]
        cands.sort(key=lambda p: p["mergedAt"], reverse=True)
        qual, fetched, log = [], 0, []
        os.makedirs(os.path.join(out, "under"), exist_ok=True)
        for p in cands:
            multi = len(rounds_of(p.get("reviews"), p["headRefOid"])) >= 2
            if len(qual) >= PER_REPO:
                if sum(1 for q in qual if q["multi"]) >= MULTI_PER_REPO:
                    break
                if not multi:
                    continue
            path = os.path.join(out, f"{tag}{p['number']}.diff")
            small = os.path.join(out, "under", f"{tag}{p['number']}.diff")
            if os.path.exists(small):
                log.append(f"{p['number']}: {os.path.getsize(small)} B, under the floor")
                continue
            if not os.path.exists(path):
                if fetched >= MAX_DIFFS_PER_REPO:
                    log.append(f"{p['number']}: diff fetch cap reached")
                    break
                fetched += 1
            data, err = cached(path, lambda: budget.gh(["pr", "diff", str(p["number"]), "-R", repo]))
            if data is None:
                log.append(f"{p['number']}: diff failed: {err}")
                continue
            if len(data) < MIN_BYTES:
                log.append(f"{p['number']}: {len(data)} B, under the floor")
                os.replace(path, small)
                continue
            qual.append({"pr": p, "multi": multi, "bytes": len(data)})
        # The 4 most recent, then swap the oldest one-round picks for multi-round ones.
        picks = qual[:PER_REPO]
        extra = [q for q in qual[PER_REPO:] if q["multi"]]
        while sum(1 for q in picks if q["multi"]) < MULTI_PER_REPO and extra:
            singles = [i for i, q in enumerate(picks) if not q["multi"]]
            if not singles:
                break
            picks[singles[-1]] = extra.pop(0)
        picks.sort(key=lambda q: q["pr"]["mergedAt"], reverse=True)
        manifest["repos"][repo] = {"candidates": len(cands), "log": log}
        for q in picks:
            p = q["pr"]
            n = p["number"]
            name = f"{tag}{n}"
            rounds = rounds_of(p.get("reviews"), p["headRefOid"])
            files, dropped = [], []
            for k, oid in enumerate(rounds, 1):
                if oid == p["headRefOid"]:
                    files.append({"round": k, "oid": oid, "file": f"{name}.diff"})
                    continue
                rp = os.path.join(out, f"{name}.r{k}.diff")
                data, err = cached(rp, lambda: budget.gh(["api", "-H", "Accept: application/vnd.github.diff",
                                                          f"repos/{repo}/compare/{p['baseRefOid']}...{oid}"]))
                if data is None or not data.strip():
                    if os.path.exists(rp):
                        os.remove(rp)
                    dropped.append({"round": k, "oid": oid, "why": err or "empty compare"})
                    continue
                final = open(os.path.join(out, f"{name}.diff"), "rb").read()
                if len(data) > len(final) and diff_files(data) - diff_files(final):
                    dropped.append({"round": k, "oid": oid, "why": "another base"})
                    continue
                files.append({"round": k, "oid": oid, "file": f"{name}.r{k}.diff"})
            vp = os.path.join(out, f"{name}.view.json")
            data, err = cached(vp, lambda: budget.gh(["pr", "view", str(n), "-R", repo, "--json", VIEW_FIELDS]))
            if data is None:
                dropped.append({"view": err})
            cp = os.path.join(out, f"{name}.comments.json")
            data, err = cached(cp, lambda: budget.gh(["api", "--paginate", f"repos/{repo}/pulls/{n}/comments"]))
            if data is None:
                dropped.append({"comments": err})
            manifest["picks"].append({
                "repo": repo, "number": n, "name": name, "mergedAt": p["mergedAt"], "bytes": q["bytes"],
                "head": p["headRefOid"], "base": p["baseRefOid"], "rounds": files, "dropped": dropped,
            })
    sums = []
    names = []
    for p in manifest["picks"]:
        names += [r["file"] for r in p["rounds"]] + [f"{p['name']}.diff", f"{p['name']}.view.json", f"{p['name']}.comments.json"]
    for f in sorted(set(names)):
        if os.path.exists(os.path.join(out, f)):
            with open(os.path.join(out, f), "rb") as fh:
                sums.append(f"{hashlib.sha256(fh.read()).hexdigest()}  {f}\n")
    with open(os.path.join(out, "SHA256SUMS"), "w", newline="\n") as fh:
        fh.writelines(sums)
    manifest["set_sha256"] = hashlib.sha256("".join(sums).encode()).hexdigest()
    manifest["gh_calls"] = budget.used
    with open(os.path.join(out, "manifest.json"), "w", newline="\n") as fh:
        json.dump(manifest, fh, indent=1)
    print(f"picks {len(manifest['picks'])}, rounds {sum(len(p['rounds']) for p in manifest['picks'])}, "
          f"multi {sum(1 for p in manifest['picks'] if len(p['rounds']) >= 2)}, gh calls {budget.used}, set {manifest['set_sha256']}")


if __name__ == "__main__":
    main()
