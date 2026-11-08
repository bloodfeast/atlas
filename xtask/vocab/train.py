#!/usr/bin/env python3
# Rebuilds the vocabulary from the sources manifest.json names, and nothing else.
#
# Run it with `py -I` (no local imports): the standard library only.
#   train.py fetch DIR            download each pinned source into DIR
#   train.py pin DIR              (first time) record each source's text hash
#   train.py verify DIR           check every hash, print them
#   train.py train DIR OUTDIR     train every declared split and mix, write merges
#   train.py frozen DIR OUT       train the frozen choice, write the vocabulary
#
# There is no argument that names a text file. The only text read is the tar
# members of a manifest source, matched by its `select` patterns, hash-checked
# and with a licence on the allowed list; anything else stops the run. So
# transcripts and the memory store cannot enter. Paths and ids are generated:
# paths from the sources' file names, ids from a seeded generator.
#
# Deterministic: sources in manifest order, members by the SHA-256 of their
# name, integer frequencies, and among equal frequencies the greatest pair of
# byte strings. A token reached twice keeps its first rank.
import collections, hashlib, json, os, random, re, sys, tarfile, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
MANIFEST = os.path.join(HERE, "manifest.json")
ALLOWED = {"MIT", "Apache-2.0", "BSD-3-Clause", "MIT OR Apache-2.0", "Unlicense OR MIT"}
CLASSES = ("prose", "code", "paths", "ids")
MAX_PIECES = 200_000

# Byte classes. Every byte of 0x80 and up counts as a letter, so the split needs
# no Unicode table and reads the same in any language.
L = rb"A-Za-z\x80-\xff"
D = rb"0-9"
W = rb" \t\n\r\x0b\x0c"
H = rb" \t\x0b\x0c"
P = rb"\x00-\x08\x0e-\x1f!-/:-@\[-`{-\x7f"
_ws = rb"[" + W + rb"]*[\r\n]|[" + H + rb"]+(?= [^" + W + rb"])|[" + H + rb"]+"
SPLITS = {
    # A run of one class, with one leading space, and digits three at a time.
    "classes": re.compile(rb" ?[" + L + rb"]+| ?[" + D + rb"]{1,3}| ?[" + P + rb"]+|" + _ws),
    # The same, and the punctuation just before letters leads them: `/src`, `.rs`.
    "lead": re.compile(rb" ?[" + L + rb"]+|[" + P + rb"][" + L + rb"]+| ?[" + D + rb"]{1,3}| ?[" + P + rb"]+(?=[" + P + rb"][" + L + rb"])| ?[" + P + rb"]+|" + _ws),
}

# The `abs:line:col` rule, from the form's definition and nothing else. The form is
# one word (a maximal run with no whitespace) that starts at a root needing no
# working directory (`/`, `~/`, `~\` or a drive letter with its slash) and ends in
# `:line:col`, both decimal. Line and column are numbers, not words: no merge is
# trained for one, so inside this suffix each group of up to three digits, as the
# split would cut it, is one token, and each colon is one. The suffix enters no
# merge and no training; the word before it is split as ever.
FORM = re.compile(rb"(?<![^" + W + rb"])(?:/|~[/\\]|[A-Za-z]:[/\\])[^" + W + rb"]*?(:[0-9]+:[0-9]+)(?![^" + W + rb"])")


def outside_form(text):
    """The stretches of `text` outside every form suffix, and the suffixes' tokens."""
    parts, tokens, at = [], 0, 0
    for g in FORM.finditer(text):
        parts.append(text[at : g.start(1)])
        at = g.end(1)
        line, col = g.group(1)[1:].split(b":")
        tokens += 2 + (len(line) + 2) // 3 + (len(col) + 2) // 3
    parts.append(text[at:])
    return parts, tokens


def manifest():
    m = json.load(open(MANIFEST, encoding="utf-8"))
    for name, s in m["sources"].items():
        if s["spdx"] not in ALLOWED:
            sys.exit(f"{name}: licence {s['spdx']} is not on the allowed list")
        if not s["url"].startswith("https://codeload.github.com/" + name + "/tar.gz/" + s["commit"]):
            sys.exit(f"{name}: the url does not pin the commit")
    return m


def tar_path(fetch, name):
    return os.path.join(fetch, name.replace("/", "__") + ".tar.gz")


def source_texts(fetch, name, s, caps):
    """The class texts of one source, and every file name in it."""
    with tarfile.open(tar_path(fetch, name), "r:gz") as t:
        ms = {m.name.split("/", 1)[1]: m for m in t if m.isreg()}
        out = {}
        for cl, pats in s["select"].items():
            rx = re.compile("|".join(pats))
            chosen = sorted((n for n in ms if rx.fullmatch(n)), key=lambda n: hashlib.sha256(n.encode()).digest())
            parts, n = [], 0
            for f in chosen:
                b = t.extractfile(ms[f]).read().replace(b"\r\n", b"\n")
                parts.append(b)
                n += len(b)
                if n >= caps[cl]:
                    break
            out[cl] = b"\n".join(parts)[: caps[cl]]
        return out, sorted(ms)


def path_text(names, r, lines):
    pre = ["", "", "./", "C:/work/repo/", "/home/user/repo/", "~/repo/", "D:\\code\\repo\\", "/usr/local/src/"]
    out = []
    for _ in range(lines):
        p = r.choice(pre)
        n = r.choice(names)
        if "\\" in p:
            n = n.replace("/", "\\")
        tail = r.choice(("", "", f":{r.randint(1, 3000)}", f":{r.randint(1, 900)}:{r.randint(1, 80)}"))
        head = r.choice(("", "", "", "M ", "?? "))
        out.append(head + p + n + tail)
    return "\n".join(out).encode()


def syllable_word(r):
    return "".join(r.choice("BDFGKLMNPRSTVZ") + r.choice("AEIOU") for _ in range(r.randint(2, 4)))


def id_text(r, lines):
    hx = lambda n: f"{r.getrandbits(4 * n):0{n}x}"
    uid = lambda: f"{hx(8)}-{hx(4)}-{hx(4)}-{hx(4)}-{hx(12)}"
    lane = lambda: f"{syllable_word(r)}-{r.choice('VBRX')}{r.randint(1, 99)}"
    b62 = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
    gens = [
        lambda: hx(7), lambda: hx(8), lambda: hx(10), lambda: hx(12), lambda: hx(16), lambda: hx(40), lambda: hx(64), uid,
        lambda: f"{r.choice(('record', 'task', 'note', 'entry'))}:{lane()}:{r.randint(1, 999):04d}@{hx(8)}",
        lambda: f"{r.choice(('record', 'task', 'note', 'entry'))}:{lane()}:{r.randint(1, 99):03d}",
        lambda: f"src/{syllable_word(r).lower()}.rs:{r.randint(1, 900)}-{r.randint(901, 1800)}@{hx(8)}",
        lambda: f"sha256:{hx(64)}",
        lambda: "".join(r.choice(b62) for _ in range(r.randint(20, 32))),
        lambda: f"#{r.randint(1, 9999)}",
    ]
    return "\n".join(r.choice(gens)() for _ in range(lines)).encode()


def class_texts(fetch, m):
    """Every class's training text, with each source's text hash."""
    caps, parts, names, hashes = m["caps"], collections.defaultdict(list), [], {}
    for name, s in m["sources"].items():
        out, ns = source_texts(fetch, name, s, caps)
        names += [f"{name.split('/')[1]}/{n}" for n in ns]
        hashes[name] = {cl: hashlib.sha256(t).hexdigest() for cl, t in out.items()}
        for cl, t in out.items():
            parts[cl].append(t)
    r = random.Random(m["seed"])
    texts = {cl: b"\n".join(parts[cl]) for cl in ("prose", "code")}
    texts["paths"] = path_text(names, r, m["generated_lines"])
    texts["ids"] = id_text(r, m["generated_lines"])
    return texts, hashes


def check(hashes, m):
    for name, s in m["sources"].items():
        if s.get("text_sha256") != hashes[name]:
            sys.exit(f"{name}: the text does not match its pinned hash")


def train(texts, split, mix, most, rule=False):
    """Merged tokens in rank order, each unique, at most `most` of them."""
    freq = collections.Counter()
    for cl, pct in zip(CLASSES, mix):
        c = collections.Counter()
        for part in outside_form(texts[cl])[0] if rule else [texts[cl]]:
            c.update(split.findall(part))
        w = pct * 10**15 // sum(c.values())
        for k, v in c.items():
            freq[k] += v * w
    items = sorted(freq.items(), key=lambda kv: (-kv[1], kv[0]))[:MAX_PIECES]
    words = [[w[i : i + 1] for i in range(len(w))] for w, _ in items]
    fr = [v for _, v in items]
    pairs, where = collections.Counter(), collections.defaultdict(set)
    for i, w in enumerate(words):
        for a, b in zip(w, w[1:]):
            pairs[(a, b)] += fr[i]
            where[(a, b)].add(i)
    merges, seen = [], set()
    while len(merges) < most and pairs:
        (a, b), v = max(pairs.items(), key=lambda kv: (kv[1], kv[0]))
        if v <= 0:
            break
        if a + b not in seen:
            seen.add(a + b)
            merges.append(a + b)
        for i in list(where[(a, b)]):
            w = words[i]
            for x, y in zip(w, w[1:]):
                pairs[(x, y)] -= fr[i]
                where[(x, y)].discard(i)
            n, j = [], 0
            while j < len(w):
                if j + 1 < len(w) and w[j] == a and w[j + 1] == b:
                    n.append(a + b)
                    j += 2
                else:
                    n.append(w[j])
                    j += 1
            words[i] = n
            for x, y in zip(n, n[1:]):
                pairs[(x, y)] += fr[i]
                where[(x, y)].add(i)
        pairs.pop((a, b), None)
        where.pop((a, b), None)
    return merges


def escape(token):
    """One vocabulary line: printable ASCII as itself, the rest and backslash as hex escapes."""
    return "".join(chr(c) if 0x20 <= c < 0x7f and c != 0x5c else f"\\x{c:02x}" for c in token)


def unescape(line):
    return re.sub(rb"\\x([0-9a-f]{2})", lambda g: bytes([int(g.group(1), 16)]), line.encode())


class Bpe:
    """The count the Rust encoder gives: same split, blocks and merge order."""

    BLOCK = 256

    def __init__(self, split, merges, rule=False):
        self.split, self.ranks, self.cache, self.rule = split, {t: i for i, t in enumerate(merges)}, {}, rule

    def _block(self, p):
        r = self.ranks
        if len(p) == 1 or p in r:
            return 1
        parts = [p[i : i + 1] for i in range(len(p))]
        while len(parts) > 1:
            best, at = None, -1
            for i in range(len(parts) - 1):
                x = r.get(parts[i] + parts[i + 1])
                if x is not None and (best is None or x < best):
                    best, at = x, i
            if best is None:
                break
            parts[at : at + 2] = [parts[at] + parts[at + 1]]
        return len(parts)

    def _piece(self, p):
        n = self.cache.get(p)
        if n is None:
            n = sum(self._block(p[i : i + self.BLOCK]) for i in range(0, len(p), self.BLOCK))
            self.cache[p] = n
        return n

    def count(self, text):
        if not self.rule:
            return sum(map(self._piece, self.split.findall(text)))
        parts, n = outside_form(text)
        return n + sum(sum(map(self._piece, self.split.findall(p))) for p in parts)


def golden_strings():
    """Strings both the trainer and the Rust test build, from one small generator."""
    atoms = ["the", " ", "a", "/", "\n", "  ", "\u00e9", "1", "22", ".rs", ":", "_", "(", ")", "\t", "\u4e2d", "X", "'", "\r\n", "\u2014", "fn", "4e7", "..", " = ", "    ", ";"]
    x, out = 12345, []
    for _ in range(400):
        s = []
        x = (x * 1103515245 + 12345) % 2**31
        for _ in range(x % 60):
            x = (x * 1103515245 + 12345) % 2**31
            s.append(atoms[(x >> 8) % len(atoms)])
        out.append("".join(s).encode())
    # The form's edges: in it, then just outside it each way.
    out += [b"/a/b.rs:12:3", b"x /a/b.rs:1234:5678\n/c:1:2 y", b"C:/a.rs:1:2", b"D:\\a\\b.rs:7:88", b"~/a.rs:9:9", b"~\\a.rs:9:9"]
    out += [b"a/b.rs:12:3", b"/a/b.rs:12", b"/a/b.rs:12:3:", b"/a/b.rs:12:3x", b"/a/b.rs:x:3", b"(/a/b.rs:12:3)", b"/a:1:2:3:4", b"C:a.rs:1:2"]
    return out


def main():
    cmd, fetch = sys.argv[1], sys.argv[2]
    m = manifest()
    if cmd == "fetch":
        os.makedirs(fetch, exist_ok=True)
        for name, s in m["sources"].items():
            if not os.path.exists(tar_path(fetch, name)):
                urllib.request.urlretrieve(s["url"], tar_path(fetch, name))
        return
    texts, hashes = class_texts(fetch, m)
    print("training bytes", {k: len(v) for k, v in texts.items()}, flush=True)
    if cmd == "pin":
        for name, s in m["sources"].items():
            s["text_sha256"] = hashes[name]
        json.dump(m, open(MANIFEST, "w", encoding="utf-8", newline="\n"), indent=1)
        return
    check(hashes, m)
    if cmd == "verify":
        print("every source matches its hash")
    elif cmd == "train":
        out = sys.argv[3]
        os.makedirs(out, exist_ok=True)
        for sp in m["splits"]:
            for mix in m["mixes"]:
                merges = train(texts, SPLITS[sp], mix, max(m["sizes"]))
                name = f"{sp}-{'_'.join(map(str, mix))}.txt"
                open(os.path.join(out, name), "w", encoding="ascii", newline="\n").write("\n".join(map(escape, merges)) + "\n")
                print(name, len(merges), flush=True)
    elif cmd == "frozen":
        f = m["frozen"]
        rule = f.get("rule") == "abs-line-col"
        merges = train(texts, SPLITS[f["split"]], f["mix"], f["size"], rule)
        open(sys.argv[3], "w", encoding="ascii", newline="\n").write("\n".join(map(escape, merges)) + "\n")
        b = Bpe(SPLITS[f["split"]], merges, rule)
        g = [b.count(s) for s in golden_strings()]
        h = 0
        for n in g:
            h = (h * 31 + n) % 2**32
        print("merges", len(merges), "golden total", sum(g), "golden hash", h)


if __name__ == "__main__":
    sys.dont_write_bytecode = True
    main()
