# Memory and rule files

**Experimental, off by default.** Work on this feature continues. See [Measured](#measured) and [Known limits](#known-limits) before turning it on.

atlas can manage the rules and memories you already keep: Claude Code's memory folder, a root `CLAUDE.md`, and a rules file you name. Nothing asks you about it, at setup or in a session: turning it on is a deliberate `atlas settings set memory-rules true`, and `atlas settings off memory-rules` goes back to the default. Nothing here adds a workflow: you keep writing the same files.

How the store and hooks work in general is in [[Memory store and hooks]]. The generated `MEMORY.md` is in [[Memory tree]].

## The setting

| `memory.rules` | Set by | What happens |
| --- | --- | --- |
| unset | the default, and `atlas settings off memory-rules` | What your configuration names by hand (`memory_dir`, `intent`) is synced and recalled as it always was. Nothing new is read. |
| `true` | `atlas settings set memory-rules true` | The sections below apply. |
| `false` | `atlas settings set memory-rules false` | The same as unset: what your configuration names by hand is synced and recalled as it always was. (Before v0.5.0, `false` stopped that too; that is now `"off"`.) |
| `"off"` | `atlas settings set memory-rules off`, by hand only | More than unset: even what your configuration names by hand stops. Memory folders and rule files stop syncing and leave the sources, and query recall and the prompt hook leave out rule and memory records. **The store is kept**: `recall {key}` still reads every record, and a later `true` picks up where it stopped. The store refuses deletes, so atlas never offers to remove it. |

## What `true` reads and writes

- **Reads.** Claude Code's memory folder for this project (`<claude>/projects/<folder>/memory`), the first `CLAUDE.md` at the working folder or a project root, and whatever `memory_dir` and `intent` already name. A sync reads these as they change.
- **Writes.** Only atlas's own store. A sync never edits a rule file. `MEMORY.md` is rewritten only if you configure `memory.tree`, which is a separate choice.
- **Not touched.** Journal entries and handoffs are out of scope: their status drives the work queue.

If you added a folder as a source, its Markdown, `CLAUDE.md` included, is already searchable with `search`. That is a separate answer; it does not put rules in the store.

## Replaced rulings

When a later ruling replaces an older one, a query that hits the older one answers with the later one, tagged:

```text
- rules:r00002: 01-04 the widget cache grows freely [replaces rules:r00001]
```

The older text stays one `recall {key}` away. Nothing is rewritten: a mark is an appended link, and `atlas rules unmark KEY` takes back a whole mark.

There are two relations, because a later ruling often changes only part of an earlier one:

- **Whole.** The older record becomes `superseded`. A query answers with the record that holds now, following a chain to its end.
- **In part.** Both stay active and link to each other. A hit on the older record shows the later one first, then the older, each saying which amends which.

A record that matches a query better does not take a ruling's place. A hit holding at least `recall.min_terms` of the query's words on a replaced or linked ruling leads the answer, by query and at the prompt hook, ahead of anything that matches better. A query also keeps the two best other rules and memories holding those words, as pointers if nothing else, past the row limit, the score floor and the byte budget; those two keep their rank, since nothing says they hold over what ranks above them. The prompt hook's gate still decides what it injects, and unset takes none of this.

A reversal cannot be guessed safely: one wrong mark hides a live ruling. So:

- A found pair (the text names a key, or quotes words only one other rule holds) is linked and stays active. It is never marked `superseded` on its own.
- Everything else is **asked**. Whole-or-part always comes from you.

## Asking: the sitting

`atlas rules` lists what reads like a reversal on your existing files, once, in two parts:

```text
atlas rules counts                 how many questions each part holds
atlas rules sitting 1              part 1, rules
atlas rules sitting 2              part 2, memories
atlas rules link                   writes the links for pairs the text names
atlas rules answer KEY --whole OLD   KEY replaces OLD
atlas rules answer KEY --partial OLD KEY changes part of OLD
atlas rules answer KEY             not a reversal
atlas rules unmark OLD             OLD is active again
```

`--add KEY` adds a record the word list does not reach. An answered question is not asked again.

A candidate comes from a short list of words (reverses, supersedes, amends, scrapped and so on). The word list misses some reversals on purpose: words like "replaces" mostly describe code. Rules it misses stay as they are, or you can add them by hand.

From then on, a new ruling can say what it replaces when it is written:

```text
atlas ruling --by NAME --supersedes rules:r00001 "the widget cache grows freely"
```

## Headlines

With `true`, the prompt hook's pointers on rules and memories are one line: the key, the date the ruling states (store time is not it: a bulk import stamps everything with the import's date) and its bold lead or first clause, instead of a fragment of the text.

```text
- rules:r00002: 01-04 the widget cache grows freely [replaces rules:r00001]
```

A headline holds less than the old pointer, so an agent may read the record one more time. The whole record is one `recall {key}` away.

## Measured

A token cut only counts after the memory fidelity eval: `cargo xtask memory-eval` as the no-loss guard, and `cargo xtask rules-eval` on a frozen set of pairs for whether the ruling that holds comes first. One scored run, off against on. These are results from a test set, not a promise.

| Bar | Result | Off | On |
| --- | --- | --- | --- |
| The ruling that holds comes first, prompt hook, of 46 | met | 19 | 33 |
| The ruling that holds comes first, query, of 46 | met | 23 | 43 |
| No loss, `memory-eval` facts, of 176 (at most 3 lost) | met | 173 | 172 |
| Tokens per question, median, off over on, at the prompt hook (bar 1.5x) | **missed** | | 1.027x |
| Tokens per question, median, off over on, by query (not a bar) | | | 0.921x: on costs more |

Beside the bars, and against turning it on: `memory-eval` strict pass fell from 98 to 82 of 100, and its mean tokens per question rose 13% (777 to 879), because a headline costs the follow-up that fetches the facts.

**The labels are circular.** The marks and the set's answers are the same answers, so a wrong mark scores as a pass. The figures show that a mark reaches the top, never that the mark is right. 21 of the 46 labels have not been reviewed by the maintainer. No negatives test a ruling wrongly displaced, so "met" does not mean safe.

## Known limits

The feature ships with these. They need the flag on, apart from the first three and what a mark made with the flag unset leaves behind (a replacement that later retires, a partial answer).

- **Marks work with the flag unset.** `atlas rules answer` and `atlas ruling --supersedes` are not gated. With the flag unset, a whole mark hides the old record from query and hook with no pointer to its successor, so a topic whose successor shares none of the query's words returns nothing; `recall {key}` still reads it. Partial links do nothing until the flag is on. Exports gain a "superseded" line on their next write, and `--memory-check` reads "differs" until then.
- **`off` edits the top-level key only.** A project's own `memory.rules` stays in force, and `atlas settings off memory-rules` may report "already off" while that project is on.
- **Going back to unset does not retire.** Records already synced from an auto-found `CLAUDE.md` or memory folder stay active and recalled, frozen at the last sync.
- **The working folder picks the rule file.** A session in a subfolder with its own `CLAUDE.md` or memory folder reads that one, and the sync retires the other's records; rules return under new keys, with marks and links left on the dead ones, and memories return on the same key when the file does.
- **`atlas rules link` can write a link backwards.** Any key near a reversal word is taken as the amended record, with no check on order, and a link leads the answer.
- **A replacement that later retires hides the old ruling too.** If the record that replaced a ruling leaves its file, a query shows neither; `recall {key}` still reads both.
- **Links do not survive an edit.** An edited item becomes a new record and the links stay on the old one, so the new one is not shown as amending and is asked about again.
- **The hook lifts more than rules and memories.** A superseded record of any recallable kind can be lifted, a handoff included, despite "Not touched" above.
- **A query's budget does not bind kept rulings.** Every replaced or linked ruling holding enough of the query's words is returned, whatever `budget_tokens` says, and each synced edit adds one.
- **A partial answer cannot be undone.** `atlas rules unmark` takes back a whole mark only; a wrong `--partial` stays, and its question is not asked again.
