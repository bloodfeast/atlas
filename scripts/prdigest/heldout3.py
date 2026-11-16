"""Pick and fetch the PR digest held-out set 3 (lane B17-FR).

The rule, fixed before any lever of this lane lands:

* The same rule as set 2 (heldout2.py) and set 1 (heldout.py): repos
  BurntSushi/ripgrep, astral-sh/ruff and cli/cli; merged before
  2026-11-08T00:00Z; no bot authors; additions + deletions >= 150; final
  diff >= 12 KB; rounds = distinct reviews[].commit.oid in order of first
  review, at most the first two and the last, a round diffed against another
  base dropped; per repo 4 picks, at least 2 with 2+ rounds (the oldest
  one-round picks give way); at most 9 diff fetches per repo.
* Set 3 looks back, not forward: it continues the rule into PRs no earlier
  set fetched. Candidates: one `gh pr list --state merged --limit 300` call
  per repo, newest merge first, after excluding every PR number that appears
  (a) in any file name under the dev folder and the set 1 folder (dropped/,
  under/ and unpicked/ included), and (b) anywhere in the set 2 manifest.json
  (excluded, picks, log) or in any file name under the set 2 folder. A number
  that only appears inside a *.list.json was listed, never fetched, and stays
  a candidate.
* "2+ rounds" counts the rounds a pick keeps: a candidate whose listing shows
  2+ reviewed commits has its round diffs fetched when it qualifies, and a
  round dropped for another base does not count. (Sets 1 and 2 counted the
  listing; two set 2 picks kept one round of three.) Round fetches are not
  diff fetches: the cap of 9 is on `gh pr diff` calls, as before.
* No list is widened and no rule relaxed when a repo runs dry: the shortfall
  is recorded in the manifest ("shortfalls"). ripgrep's is expected (set 2
  found no multi-round ripgrep PR in its 200 most recent).
* Each pick keeps its `gh pr view --json` and inline review comments, its
  per-round diffs, and heads/STEM/ (heads.py: the base .gitattributes and
  first lines of every modified or deleted file, from a blobless clone, no gh
  call).

Read-only gh calls only, counted against a hard budget; every fetch is cached
in the output folder, so a rerun spends nothing. Writes manifest.json and
SHA256SUMS (every fixture and head file) to the output folder.

Usage: py -I heldout3.py DEV_DIR SET1_DIR SET2_DIR OUT_DIR CLONES [BUDGET]
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

LIST_LIMIT = 300
LIST_FIELDS = "number,mergedAt,additions,deletions,author,baseRefOid,headRefOid,reviews"
STEM = re.compile(r"(rg|ruff|gh)(\d+)\.")


def names_under(d):
    """Every (tag, number) a file name anywhere under this folder carries."""
    seen = set()
    for _, _, names in os.walk(d):
        for n in names:
            m = STEM.match(n)
            if m:
                seen.add((m.group(1), int(m.group(2))))
    return seen


def manifest_numbers(path):
    """Every (tag, number) the set 2 manifest names: excluded, picks, logs."""
    with open(path, encoding="utf-8") as f:
        text = f.read()
    seen = {(t, int(n)) for t, n in re.findall(r"\b(rg|ruff|gh)(\d+)\b", text)}
    tags = dict(REPOS)
    for repo, info in json.loads(text).get("repos", {}).items():
        for line in info.get("log", []):
            m = re.match(r"(\d+):", line)
            if m:
                seen.add((tags[repo], int(m.group(1))))
    for p in json.loads(text).get("picks", []):
        seen.add((tags[p["repo"]], int(p["number"])))
    return seen


def fetch_rounds(out, repo, name, p, budget):
    """The rounds a PR keeps and the ones it drops, fetching each compare."""
    files, dropped = [], []
    os.makedirs(os.path.join(out, "dropped"), exist_ok=True)
    with open(os.path.join(out, f"{name}.diff"), "rb") as f:
        final = f.read()
    for k, oid in enumerate(rounds_of(p.get("reviews"), p["headRefOid"]), 1):
        if oid == p["headRefOid"]:
            files.append({"round": k, "oid": oid, "file": f"{name}.diff"})
            continue
        rp = os.path.join(out, f"{name}.r{k}.diff")
        gone = os.path.join(out, "dropped", f"{name}.r{k}.diff")
        if os.path.exists(gone):
            dropped.append({"round": k, "oid": oid, "why": "another base"})
            continue
        data, err = cached(rp, lambda: budget.gh(["api", "-H", "Accept: application/vnd.github.diff",
                                                  f"repos/{repo}/compare/{p['baseRefOid']}...{oid}"]))
        if data is None or not data.strip():
            if os.path.exists(rp):
                os.remove(rp)
            dropped.append({"round": k, "oid": oid, "why": err or "empty compare"})
            continue
        if len(data) > len(final) and diff_files(data) - diff_files(final):
            os.replace(rp, gone)
            dropped.append({"round": k, "oid": oid, "why": "another base"})
            continue
        files.append({"round": k, "oid": oid, "file": f"{name}.r{k}.diff"})
    return files, dropped


def main():
    dev, set1, set2, out, clones = sys.argv[1:6]
    budget = Budget(int(sys.argv[6]) if len(sys.argv) > 6 else 75)
    for sub in ("under", "unpicked", "dropped"):
        os.makedirs(os.path.join(out, sub), exist_ok=True)
    excluded = names_under(dev) | names_under(set1) | names_under(set2) | manifest_numbers(os.path.join(set2, "manifest.json"))
    manifest = {"rule": __doc__.split("Read-only")[0].strip(), "cutoff": CUTOFF, "list_limit": LIST_LIMIT,
                "excluded": sorted(f"{t}{n}" for t, n in excluded), "repos": {}, "picks": [], "shortfalls": []}
    for repo, tag in REPOS:
        lst, err = cached(os.path.join(out, f"{tag}.list.json"),
                          lambda: budget.gh(["pr", "list", "-R", repo, "--state", "merged", "--limit", str(LIST_LIMIT), "--json", LIST_FIELDS]))
        if lst is None:
            raise SystemExit(f"{repo} list: {err}")
        prs = json.loads(lst)
        cands = [
            p for p in prs
            if p.get("mergedAt") and p["mergedAt"] < CUTOFF
            and (tag, p["number"]) not in excluded
            and not (p.get("author") or {}).get("is_bot")
            and not (p.get("author") or {}).get("login", "").endswith("[bot]")
            and p.get("additions", 0) + p.get("deletions", 0) >= MIN_CHANGED
        ]
        cands.sort(key=lambda p: p["mergedAt"], reverse=True)
        qual, fetched, log, capped = [], 0, [], False
        listed_multi = sum(1 for p in cands if len(rounds_of(p.get("reviews"), p["headRefOid"])) >= 2)
        for p in cands:
            name = f"{tag}{p['number']}"
            listed = len(rounds_of(p.get("reviews"), p["headRefOid"])) >= 2
            if len(qual) >= PER_REPO:
                if sum(1 for q in qual if q["multi"]) >= MULTI_PER_REPO:
                    break
                if not listed:
                    continue
            path = os.path.join(out, f"{name}.diff")
            small = os.path.join(out, "under", f"{name}.diff")
            parked = os.path.join(out, "unpicked", f"{name}.diff")
            if os.path.exists(small):
                log.append(f"{p['number']}: {os.path.getsize(small)} B, under the floor")
                continue
            if os.path.exists(parked):
                os.replace(parked, path)  # a rerun: the cache of a qualifier that gave way
                for n in os.listdir(os.path.join(out, "unpicked")):
                    if n.startswith(f"{name}.r"):
                        os.replace(os.path.join(out, "unpicked", n), os.path.join(out, n))
            if not os.path.exists(path):
                if fetched >= MAX_DIFFS_PER_REPO:
                    log.append(f"{p['number']}: diff fetch cap reached")
                    capped = True
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
            files, dropped = fetch_rounds(out, repo, name, p, budget)
            multi = len(files) >= 2
            if listed and not multi:
                log.append(f"{p['number']}: listed with 2+ reviewed commits, keeps {len(files)} round(s)")
            if not files:
                log.append(f"{p['number']}: no round kept")
                os.replace(path, os.path.join(out, "dropped", f"{name}.diff"))
                continue
            qual.append({"pr": p, "multi": multi, "bytes": len(data), "rounds": files, "dropped": dropped})
        picks = qual[:PER_REPO]
        extra = [q for q in qual[PER_REPO:] if q["multi"]]
        while sum(1 for q in picks if q["multi"]) < MULTI_PER_REPO and extra:
            singles = [i for i, q in enumerate(picks) if not q["multi"]]
            if not singles:
                break
            picks[singles[-1]] = extra.pop(0)
        picks.sort(key=lambda q: q["pr"]["mergedAt"], reverse=True)
        for q in qual:
            if q not in picks:
                name = f"{tag}{q['pr']['number']}"
                for n in os.listdir(out):
                    if n == f"{name}.diff" or (n.startswith(f"{name}.r") and n.endswith(".diff")):
                        os.replace(os.path.join(out, n), os.path.join(out, "unpicked", n))
                log.append(f"{q['pr']['number']}: qualified, not picked")
        multi_n = sum(1 for q in picks if q["multi"])
        manifest["repos"][repo] = {"listed": len(prs), "candidates": len(cands), "candidates_listed_2plus": listed_multi,
                                   "candidate_numbers": [p["number"] for p in cands], "log": log, "diff_fetches": fetched,
                                   "picks": len(picks), "picks_2plus_rounds": multi_n}
        if len(picks) < PER_REPO or multi_n < MULTI_PER_REPO:
            manifest["shortfalls"].append({
                "repo": repo, "picks": len(picks), "want_picks": PER_REPO, "picks_2plus_rounds": multi_n, "want_2plus_rounds": MULTI_PER_REPO,
                "why": "diff fetch cap reached" if capped else "every candidate tried",
            })
        for q in picks:
            p = q["pr"]
            n = p["number"]
            name = f"{tag}{n}"
            dropped = list(q["dropped"])
            data, err = cached(os.path.join(out, f"{name}.view.json"), lambda: budget.gh(["pr", "view", str(n), "-R", repo, "--json", VIEW_FIELDS]))
            if data is None:
                dropped.append({"view": err})
            data, err = cached(os.path.join(out, f"{name}.comments.json"), lambda: budget.gh(["api", "--paginate", f"repos/{repo}/pulls/{n}/comments"]))
            if data is None:
                dropped.append({"comments": err})
            nh, missing, attrs = heads.write_heads(out, name, p["baseRefOid"], clones, os.path.join(out, "heads"))
            manifest["picks"].append({
                "repo": repo, "number": n, "name": name, "mergedAt": p["mergedAt"], "bytes": q["bytes"],
                "head": p["headRefOid"], "base": p["baseRefOid"], "rounds": q["rounds"], "dropped": dropped,
                "heads": {"files": nh, "not_at_base": missing, "gitattributes": attrs},
            })
    sums, names = [], []
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
          f"multi {sum(1 for p in manifest['picks'] if len(p['rounds']) >= 2)}, shortfalls {len(manifest['shortfalls'])}, "
          f"gh calls {budget.used}, set {manifest['set_sha256']}")


if __name__ == "__main__":
    main()
