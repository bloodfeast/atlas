"""Pick and fetch the PR digest held-out set 2 (PRDIGEST15X-D1, review M3).

The rule, fixed before any lever of lane D lands:

* The same rule as heldout.py (set 1): repos ripgrep, ruff and cli/cli;
  merged before 2026-11-08T00:00Z; no bot authors; additions + deletions
  >= 150; final diff >= 12 KB; rounds = distinct reviews[].commit.oid in
  order of first review, at most the first two and the last, a round diffed
  against another base dropped; per repo 4 picks, at least 2 with 2+ rounds
  (the oldest one-round picks give way); at most 9 diff fetches per repo.
* Candidates: each repo's cached set 1 list (`TAG.list.json`, the 100 most
  recently created merged PRs), newest merge first, after excluding every PR
  number that appears anywhere under the set 1 folder (picks, dropped/,
  under/, unpicked/) and in prdigest/raw and prdigest/measure.
* If a repo's list runs dry (fewer than 4 picks, or fewer than 2 with 2+
  rounds, once every candidate is tried), one more list call fetches the 200
  most recently created merged PRs, and its older 100 are tried next, under
  the same rule. The call is recorded in the manifest.
* Each pick keeps its `gh pr view --json` and inline review comments, and
  heads/STEM/ (heads.py: the base .gitattributes and first lines of every
  modified or deleted file, from a blobless clone, no gh call).

Read-only gh calls only, counted against a hard budget; every fetch is cached
in the output folder. Writes manifest.json and SHA256SUMS (every fixture and
head file) to the output folder.

Usage: py -I heldout2.py SET1_DIR OUT_DIR CLONES [BUDGET]
"""

import hashlib
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import heads  # noqa: E402
from heldout import (CUTOFF, MAX_DIFFS_PER_REPO, MIN_BYTES, MIN_CHANGED, MULTI_PER_REPO, PER_REPO, REPOS, VIEW_FIELDS,  # noqa: E402
                     Budget, cached, diff_files, rounds_of)

# The earlier PR digest fixture folders whose PR numbers are excluded. These
# are the maintainer's local scratch paths, as set 2 was picked; elsewhere,
# name them with ATLAS_PRDIGEST_OTHER_DIRS (`;` between folders).
OTHER_DIRS = [d for d in os.environ.get("ATLAS_PRDIGEST_OTHER_DIRS", "Z:/scratch/v03release/prdigest/raw;Z:/scratch/v03release/prdigest/measure").split(";") if d]


def seen_numbers(dirs):
    """Every (tag, number) named by a file anywhere under these folders."""
    seen = set()
    for d in dirs:
        for _, _, names in os.walk(d):
            for n in names:
                m = re.match(r"(rg|ruff|gh)(\d+)\.", n)
                if m:
                    seen.add((m.group(1), int(m.group(2))))
    return seen


def qualifies(p, excluded, tag, repo):
    return (p.get("mergedAt") and p["mergedAt"] < CUTOFF
            and (tag, p["number"]) not in excluded
            and not (p.get("author") or {}).get("is_bot")
            and not (p.get("author") or {}).get("login", "").endswith("[bot]")
            and p.get("additions", 0) + p.get("deletions", 0) >= MIN_CHANGED)


def main():
    set1, out, clones = sys.argv[1:4]
    budget = Budget(int(sys.argv[4]) if len(sys.argv) > 4 else 79)
    os.makedirs(os.path.join(out, "under"), exist_ok=True)
    excluded = seen_numbers([set1] + OTHER_DIRS)
    manifest = {"rule": __doc__.split("Read-only")[0].strip(), "cutoff": CUTOFF, "excluded": sorted(f"{t}{n}" for t, n in excluded),
                "repos": {}, "picks": []}
    for repo, tag in REPOS:
        with open(os.path.join(set1, f"{tag}.list.json"), "rb") as f:
            first = json.load(f)
        lists = [("set 1 list", first)]
        qual, fetched, log, tried = [], 0, [], set()

        def done():
            return len(qual) >= PER_REPO and sum(1 for q in qual if q["multi"]) >= MULTI_PER_REPO

        for li in range(2):
            if li == 1:
                if done():
                    break
                data, err = cached(os.path.join(out, f"{tag}.list200.json"),
                                   lambda: budget.gh(["pr", "list", "-R", repo, "--state", "merged", "--limit", "200",
                                                      "--json", "number,mergedAt,additions,deletions,author,baseRefOid,headRefOid,reviews"]))
                if data is None:
                    log.append(f"older list failed: {err}")
                    break
                known = {p["number"] for p in first}
                older = [p for p in json.loads(data) if p["number"] not in known]
                lists.append(("older 100 (one list call, --limit 200)", older))
                log.append(f"list ran dry; older list: {len(older)} PRs")
            name, prs = lists[-1]
            cands = [p for p in prs if qualifies(p, excluded, tag, repo) and p["number"] not in tried]
            cands.sort(key=lambda p: p["mergedAt"], reverse=True)
            log.append(f"{name}: {len(cands)} candidates")
            for p in cands:
                tried.add(p["number"])
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
            if fetched >= MAX_DIFFS_PER_REPO:
                break
        picks = qual[:PER_REPO]
        extra = [q for q in qual[PER_REPO:] if q["multi"]]
        while sum(1 for q in picks if q["multi"]) < MULTI_PER_REPO and extra:
            singles = [i for i, q in enumerate(picks) if not q["multi"]]
            if not singles:
                break
            picks[singles[-1]] = extra.pop(0)
        picks.sort(key=lambda q: q["pr"]["mergedAt"], reverse=True)
        os.makedirs(os.path.join(out, "unpicked"), exist_ok=True)
        for q in qual:
            if q not in picks:
                n = f"{tag}{q['pr']['number']}.diff"
                os.replace(os.path.join(out, n), os.path.join(out, "unpicked", n))
        manifest["repos"][repo] = {"log": log, "diff_fetches": fetched}
        for q in picks:
            p = q["pr"]
            n = p["number"]
            name = f"{tag}{n}"
            rounds = rounds_of(p.get("reviews"), p["headRefOid"])
            files, dropped = [], []
            os.makedirs(os.path.join(out, "dropped"), exist_ok=True)
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
                    os.replace(rp, os.path.join(out, "dropped", f"{name}.r{k}.diff"))
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
            nh, missing, attrs = heads.write_heads(out, name, p["baseRefOid"], clones, os.path.join(out, "heads"))
            manifest["picks"].append({
                "repo": repo, "number": n, "name": name, "mergedAt": p["mergedAt"], "bytes": q["bytes"],
                "head": p["headRefOid"], "base": p["baseRefOid"], "rounds": files, "dropped": dropped,
                "heads": {"files": nh, "not_at_base": missing, "gitattributes": attrs},
            })
    sums = []
    names = []
    for p in manifest["picks"]:
        names += [r["file"] for r in p["rounds"]] + [f"{p['name']}.diff", f"{p['name']}.view.json", f"{p['name']}.comments.json"]
    for f in sorted(set(names)):
        if os.path.exists(os.path.join(out, f)):
            with open(os.path.join(out, f), "rb") as fh:
                sums.append(f"{hashlib.sha256(fh.read()).hexdigest()}  {f}\n")
    hroot = os.path.join(out, "heads")
    hs = []
    for p in manifest["picks"]:
        for root, _, files in os.walk(os.path.join(hroot, p["name"])):
            for fn in files:
                fp = os.path.join(root, fn)
                with open(fp, "rb") as fh:
                    hs.append(f"{hashlib.sha256(fh.read()).hexdigest()}  heads/{os.path.relpath(fp, hroot).replace(os.sep, '/')}\n")
    sums += sorted(hs, key=lambda l: l[66:])
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
