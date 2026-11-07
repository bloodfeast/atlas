# Declared before any score (2026-11-06)

Rulings: ATLAS-V03 0285, 0287, 0293, and the review's edits. This file was
committed before any vocabulary was trained or scored. Nothing below moves after
a result is seen. A miss stops and goes back to the project owner.

## Training

- Sources: `manifest.json`, pinned by commit, with an SPDX id and a SHA-256 of
  the selected text per source and class. Text is fetched to scratch and never
  committed. Paths are generated from the sources' file names, ids from a seeded
  generator. `train.py` takes the manifest and reads nothing else.
- Splits, written here from byte classes (letters including every byte of 0x80
  and up; digits; ASCII whitespace; the rest punctuation):
  - `classes`: a run of one class with at most one leading space, digits three
    at a time, whitespace up to its last newline as one piece, and the last
    space before text leading the text.
  - `lead`: the same, and the punctuation byte just before letters leads them.
- Mixes, prose/code/paths/ids, in percent, chosen from what session text is:
  40/40/10/10 (balanced), 30/30/20/20 (key-dense, as cards are), 30/50/10/10
  (code-heavy, as Read output is).
- Sizes: 300, 500, 744, 1000, 2000 merged tokens, as prefixes of one training.

## Rows

Same rows file as the first proof (`rows.py`, its classes unchanged). The Bash constant
was checked once on Bash rows 10-10 to 10-23: intercept 581 tokens with
`total_tokens_reminder` (n=6,138), 10 with no attachment (n=154). It tracks the
reminder, so rows with the reminder are excluded, not modelled, and no per-tool
overhead is removed.

Clean row: Opus 5.5 both sides, under 200 output tokens, no reminder, and either
no attachment or `hook_success` only, at most 5% of the row's bytes. The reminder
ends 10-23, so clean rows start there. Split by time at the median clean classed
row, 2026-10-31: early rows are dev, late rows are of files early rows never saw.
Late rows by class: prose 92, code 116, paths 9, ids 1. Under 20 reads not
measured, so paths and ids have no row verdict.

## Choosing, on dev data only

Dev data is counted set 1 and early rows. Each candidate (split, mix, size) gets
a class factor from set 1. Dev error per class: prose and code the median on early
Read rows; paths, ids and card the leave-one-out median on set 1. Choose the
lowest worst-class dev error. Within 1.0 point of the lowest, take the smallest
size (footprint, ruling 0278), then `classes` before `lead`, then the mix order
above. The choice is then frozen in `manifest.json`, with the factors from set 1.
The leak check runs on it before any verdict score. A hit stops.

## The verdict, scored once on the frozen choice

| Class | Verdict set | Spread |
|---|---|---|
| prose | late clean Read rows | p90 |
| code | late clean Read rows | p90 |
| paths | set 3, 12 texts | max |
| ids | set 3, 12 texts | max |
| card | set 3, 6 texts | max |

Set 3 (30 texts, SHA-256 over sorted names and bytes
f6b8930d6d38a3b368a51a1a06ecea2a59f2083d5b93b183a451c9ca306e2834) was written
before training, in scratch: paths from six public trees the manifest does not
name, ids and cards invented. Its 8-gram overlap with the training text is
reported.

Flat is bytes times the class's bytes-per-token from set 1, on the same rows or
texts. A class passes only with all three, unrounded:

1. median absolute error at most 10%;
2. median lower than flat's (a tie fails);
3. spread at most two thirds of flat's.

Any class failing is a miss of the whole vocabulary. Reported beside the verdict
and never gating: a file-level bootstrap interval on rows, set 2, `other` rows,
and a public vocabulary as a comparison from scratch.

## Paid calls

At most 34 `claude -p` counting calls: set 3 (30), a base text (2), the fixture
card (up to 2). No others.

# Second declaration, paths factor only, before any count of its sets (2026-11-06)

The first verdict below the line above missed on paths and card and stands as
scored. The project owner ruled one more declared try, after seeing that result:
a paths factor from a larger counted set, scored once on a fresh set. Nothing below
moves after a result is seen. A paths miss stops and goes back to him.

## Frozen, unchanged

`vocab-500.txt`, split `lead`, every other class's factor, the trainer. Prose, code
and ids stand as scored in the first verdict and are not rescored as verdicts.

## Fit set, paths only

All counted paths texts: set 1 (6), set 2 (6), set 3 (12), all already seen, and a
new `fit4` of 12, 36 in all. None is dropped, whatever its error. `fit4` has the
same 12 forms as set 3, from six public trees named before counting:
  - grafana/grafana b9e28156159b (24713 files)
  - rails/rails 099028e19744 (5031 files)
  - apache/kafka 85bcc7780c50 (7663 files)
  - pytorch/pytorch 4d6bffb166fc (22674 files)
  - microsoft/vscode c0a88d240be7 (20931 files)
  - bitcoin/bitcoin 411c971e75ba (3082 files)

Text is file names only, SHA-256 over sorted names and bytes
af7181fe787625bd53e5e6a0579b229c304575fa70972a27682f85afbd10e5be.

The factor is the median over the 36 texts of real tokens over vocabulary count,
the statistic set 1's factor used. Flat's paths factor is refitted the same way, the
median of real tokens over bytes on the same 36 texts: the vocabulary must beat flat
on equal terms, so ours does not get a larger fit than the flat it is compared with.
Each batch's real count subtracts its own base text. Both factors are committed to
`manifest.json` before any verdict text is counted.

## Verdict set 4

18 texts, never counted, SHA-256 bc28047970326fc43dd3989d4aac71c0a8e45d12479bc0585c01181d9959399e:
12 paths, the same 12 forms as set 3, from six other public trees:
  - golang/go 739e336c723d (16183 files)
  - postgres/postgres 72b9498df9a3 (7726 files)
  - ansible/ansible 7dbd135e0cc1 (5918 files)
  - elastic/elasticsearch a97873291f9a (38486 files, tree listing truncated)
  - WordPress/WordPress cce535093605 (5325 files)
  - flutter/flutter 09175cdeac89 (16319 files)

and 6 cards, invented, the set 3 template with new names and notes. Disjoint: no
named tree is in the training manifest or sets 1 to 3; a file name found in the
training paths text, sets 1 to 3 or `fit4` is redrawn; no line of set 4 equals a
line of sets 1 to 3, except the two fixed template lines every card carries.
8-gram overlap with training: paths 3873/28287 = 13.7%, card
256/2618 = 9.8% (set 3 read 14.0% and 9.5%).

## The bar

Paths, scored once on set 4's 12 texts, unrounded, as the first verdict's:

1. median absolute error at most 10%;
2. median lower than flat's (a tie fails);
3. max at most two thirds of flat's max.

Flat here is the refitted flat. Beside it, never gating: flat with its set 1 factor,
and ours with its set 1 factor.

Card, 6 texts of set 4, on the unchanged card factors from set 1: median absolute
error at most 10%, and that alone. The owner dropped beats-flat and the spread test
for the card only, by a ruling made after seeing the first result; every report of
this row says so and shows flat beside it. The first verdict's card row stands
beside it, unchanged.

## Paid calls

At most 34 `claude -p` counting calls in total: a base text (2), `fit4` (12), set 4
(18), the fixture card (up to 2). The counter refuses more and stops at the cap.

# Third declaration, one split rule for the `abs:line:col` form, before any count (2026-11-06)

The second verdict missed on the paths max, at this form (+32.3%). The project owner
ruled one more declared try, after seeing that result and being told a third try at
a seen failure can fit to the test. Nothing below moves after a result is seen. A miss
stops and goes back to him, with no further try.

## The rule

The form: one word (a maximal run with no ASCII whitespace) that starts at a root
needing no working directory (`/`, `~/`, `~\`, or a drive letter, a colon and a slash
of either kind) and ends in `:line:col`, both decimal, at the word's end. `FORM` in
`train.py` is exactly this, and nothing else is matched: a relative path, a word with
anything after the column, and a word in brackets are all outside it.

Inside the suffix, each colon is one token and each group of up to three digits, cut
as the split cuts digits, is one token. They reach no merge. The word before the suffix
is split as ever. It is written from what line and column are: numbers, for which the
vocabulary holds no merges worth the name (11 digit tokens in 500), so an unmerged
three-digit number counts three. No counted text was read to write it: the reason came
from the second verdict's per-text errors and from hand-written strings on the
vocabulary, and those are named so the reader can weigh it. No constant in the rule is
fitted; no other form, relative `path:line:col` included, gets a treatment.

## What is refitted, mechanically, with no new choice

1. Retrain the frozen choice (split `lead`, mix 40/40/10/10, 500 merges, same manifest,
   same tie-break) with the suffix outside the training text. If the merges come out
   different, the result replaces `vocab-500.txt`; if not, the file stands. Either way
   no other size, mix or split is looked at.
2. Every factor is refitted by its own statistic on the same texts it used, all already
   seen: prose, code, ids and card, the median over set 1's six texts of real over
   count; paths, the median over the same 36 texts as the second declaration (sets 1, 2,
   3 and `fit4`). Set 4 stays out of the fit, so flat's factor is the bar's as it was.
3. Flat's factors do not change: bytes do not depend on the rule. Paths flat is 0.5265
   (refitted, as the second declaration), and 0.5897 is its set 1 factor.
4. The factors are committed to `manifest.json` before any text of set 5 is counted,
   and the leak check runs on the vocabulary before any verdict score. A hit stops.

## Verdict set 5

26 texts, never counted, SHA-256 1586ce8893e62e029f6574c1dc7056d1b3a136089453cf7856154fa10e2fa6a5,
file names only and invented cards, kept in scratch:

- 12 `paths`, the same 12 forms as sets 3 and 4, one of them the form;
- 8 `form`, all of the form, one root each: `/opt/checkout/`, `/home/dev/work/`,
  `/Users/dev/work/`, `/tmp/build-cache/`, `C:/work/tree/`, `H:\builds\tree\`, `~/src/`,
  `/mnt/ci/workspace/`; line 1 to 4000, column 1 to 120;
- 6 `card`, the template of sets 3 and 4, new names and notes.

From six public trees named before counting, in no earlier set and not in the manifest:
  - nodejs/node a10b7ecfdfc1 (56818 files)
  - swiftlang/swift 369b6f0b2ff3 (33773 files)
  - moby/moby 402d0a16f131 (13469 files)
  - mastodon/mastodon 4dd3a453750e (10133 files)
  - openjdk/jdk 25ce7c2cbba3 (46660 files, tree listing truncated)
  - git/git 0ffa3e35c7c8 (4863 files)

A file name found in the training paths text, sets 1 to 4 or `fit4` is redrawn. No line
of set 5 equals a line of an earlier set, except the two fixed template lines every card
carries. 8-gram overlap with training: paths 3849/33400 = 11.5%, form 2998/23176 = 12.9%,
card 249/2577 = 9.7%. The rule matches every line of the form texts and of `paths_04`,
and no line of any other text of the set.

## The bar, scored once on set 5

Paths bar as in the second verdict, unrounded, each on the same texts for ours and flat:

1. median absolute error at most 10%;
2. median lower than flat's (a tie fails);
3. max at most two thirds of flat's max.

It applies to each of three rows, and any row failing is a miss:

| Row | Texts | Why |
|---|---|---|
| paths | 20: the 12 and the 8 | the form within paths |
| paths-12 | the 12 forms alone | the composition of sets 3 and 4 |
| form | 9: the 8 and `paths_04` | the form as its own row |

Which flat: the limit is computed from flat at 0.5265, the refitted factor, as the second
verdict's was. Each row is also read with flat at its set 1 factor, 0.5897. That choice
must not decide the result: if the two readings disagree on any row, it is a miss, and
the report says so.

Card, 6 texts of set 5, on the refitted card factor, flat at 0.4971 beside it: median
absolute error at most 10%, and that alone. The owner dropped beats-flat and the spread
test for the card only, by a ruling made after seeing the first result; every report of
this row says so.

## Rescore of prose, code and ids

The split changes, so these are scored once more, with the rule and the refitted
factors, on the verdict rows of the first declaration: 92 prose and 116 code late clean
Read rows (p90), 12 ids of set 3 (max). Those are the texts the rule could touch, and
retraining can touch every one of them. Their limits stand as first scored: flat's medians
20.75, 15.46, 8.59 and spreads 18.72, 15.11, 25.14. Any row past its bar is a miss. The
number of texts the rule itself matches in each row is reported beside it.

## Paid calls

At most 34 `claude -p` counting calls in total: a base text (2), set 5 (26), the fixture
card (up to 2). No others. The counter refuses more and stops at the cap.
