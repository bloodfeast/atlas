// Freeze the IDEA-CAPTURE corpus from top-level Claude Code transcripts.
//
//   node freeze.mjs --project DIR --filter redact_filter.exe --out DIR --cutoff ISO \
//        [--prior FILE]... [--heldout-frac 0.35]
//
// Three strata: `typed` (origin human), `queued` (typed during a turn, seen only as a
// queued_command attachment), `ask` (AskUserQuestion answers that are not option labels).
// Every text goes through atlas's redaction. Exact repeats collapse to the first
// occurrence. The split is by time. Stdout carries counts and hashes, never text.
import fs from 'fs';
import path from 'path';
import rl from 'readline';
import crypto from 'crypto';
import { spawnSync } from 'child_process';

const argv = process.argv.slice(2);
const opt = (k, d) => { const i = argv.indexOf('--' + k); return i < 0 ? d : argv[i + 1]; };
const many = k => argv.flatMap((a, i) => (a === '--' + k ? [argv[i + 1]] : []));
const project = opt('project'), filter = opt('filter'), out = opt('out'), cutoff = opt('cutoff');
const heldFrac = Number(opt('heldout-frac', '0.35'));
const sha = b => crypto.createHash('sha256').update(b).digest('hex');
const norm = s => s.toLowerCase().replace(/\s+/g, ' ').trim();
const textOf = c => typeof c === 'string' ? c : Array.isArray(c) ? c.filter(b => b.type === 'text').map(b => b.text).join('\n') : '';
const isTyped = s => s !== '' && !/^\/[a-z-]+(\s|$)/.test(s) && !s.startsWith('<command-') && !s.startsWith('<local-command');

const typed = [], queued = [], ask = [];
const seenUuid = new Set();
const files = fs.readdirSync(project).filter(f => f.endsWith('.jsonl')).sort();
for (const f of files) {
  const session8 = f.slice(0, 8);
  for await (const l of rl.createInterface({ input: fs.createReadStream(path.join(project, f)) })) {
    let o;
    try { o = JSON.parse(l); } catch { continue; }
    if (o.isSidechain) continue;
    if (o.type === 'user') {
      if (o.uuid) { if (seenUuid.has(o.uuid)) continue; seenUuid.add(o.uuid); }
      const ts = o.timestamp;
      if (!ts || ts > cutoff) continue;
      if (o.origin?.kind === 'human' && !o.isMeta) {
        const s = textOf(o.message?.content).trim();
        if (isTyped(s)) typed.push({ stratum: 'typed', ts, session8, text: s });
      }
      const r = o.toolUseResult;
      if (r && r.answers && Array.isArray(r.questions)) {
        for (const [q, v] of Object.entries(r.answers)) {
          const labels = (r.questions.find(x => x.question === q)?.options || []).map(x => x.label);
          const a = String(v);
          if (labels.includes(a) || a.split(/,\s*/).every(p => labels.includes(p))) continue;
          if (a.trim()) ask.push({ stratum: 'ask', ts, session8, text: a.trim() });
        }
      }
    } else if (o.type === 'attachment' && o.attachment?.type === 'queued_command') {
      const a = o.attachment, ts = a.timestamp || o.timestamp;
      if (a.origin?.kind !== 'human' || a.isMeta || !ts || ts > cutoff) continue;
      const s = textOf(a.prompt).trim();
      if (isTyped(s)) queued.push({ stratum: 'queued', ts, session8, text: s });
    }
  }
}

// Redact first, so nothing below or on disk ever holds a secret-shaped line.
const all = [...typed, ...queued, ...ask];
const esc = s => s.replace(/\\/g, '\\\\').replace(/\r/g, '\\r').replace(/\n/g, '\\n');
const unesc = s => s.replace(/\\(.)/g, (_, c) => (c === 'n' ? '\n' : c === 'r' ? '\r' : c));
const run = spawnSync(filter, { input: all.map(r => esc(r.text)).join('\n') + '\n', encoding: 'utf8', maxBuffer: 1 << 30 });
if (run.status !== 0) throw new Error('redact filter failed: ' + run.status);
const lines = run.stdout.split('\n');
if (lines.length !== all.length + 1) throw new Error('redact filter line count');
// Each output line is `<by rule><by term>\t<text>`. Secret rules blank lines; a denylist
// term goes by span only, so this corpus differs from what live redaction would keep.
let byRule = 0, byTerm = 0;
all.forEach((r, i) => {
  const flags = lines[i].slice(0, 2);
  if (flags[0] === '1') { byRule++; r.redacted = true; }
  if (flags[1] === '1') { byTerm++; r.redacted = true; r.term = true; }
  r.text = unesc(lines[i].slice(3));
});

// A typed prompt wins over its queued copy; a repeat collapses onto its first time.
const rank = { typed: 0, queued: 1, ask: 2 };
const typedTexts = new Set(all.filter(r => r.stratum === 'typed').map(r => norm(r.text)));
const stream = {};
for (const r of all) stream[r.stratum] = (stream[r.stratum] || 0) + 1;
all.sort((a, b) => (a.ts < b.ts ? -1 : a.ts > b.ts ? 1 : rank[a.stratum] - rank[b.stratum]));
const byNorm = new Map();
let queuedEcho = 0, repeats = 0;
for (const r of all) {
  const n = norm(r.text);
  if (r.stratum === 'queued' && typedTexts.has(n)) { queuedEcho++; continue; }
  if (byNorm.has(n)) { repeats++; byNorm.get(n).repeats++; continue; }
  r.repeats = 0;
  byNorm.set(n, r);
}
const rows = [...byNorm.values()];
for (const r of rows) r.id = 'i' + sha(norm(r.text)).slice(0, 12);

// The newest fraction, on one time boundary across every stratum.
const cut = Math.floor(rows.length * (1 - heldFrac));
const boundary = rows[cut].ts;
const tune = rows.slice(0, cut), held = rows.slice(cut);

// Prior positives: rows whose words stand quoted in a source older than any detector.
const quotes = new Set();
for (const p of many('prior')) {
  for (const m of fs.readFileSync(p, 'utf8').matchAll(/["“]([^"“”\n]{20,})["”]/g)) quotes.add(norm(m[1]));
}
const prior = [];
const qList = [...quotes];
for (const r of rows) {
  const n = norm(r.text);
  if (qList.some(q => n.includes(q) || (n.length >= 20 && q.includes(n)))) prior.push(r.id);
}

const dump = rs => rs.map(r => JSON.stringify({ id: r.id, stratum: r.stratum, ts: r.ts, session8: r.session8, repeats: r.repeats, redacted: !!r.redacted, text: r.text })).join('\n') + '\n';
fs.mkdirSync(out, { recursive: true });
const write = (name, data) => { fs.writeFileSync(path.join(out, name), data); return { bytes: Buffer.byteLength(data), sha256: sha(data) }; };
const m = { cutoff, boundary, held_frac: heldFrac, transcripts: files.length, files: {} };
m.files['corpus.tune.jsonl'] = write('corpus.tune.jsonl', dump(tune));
m.files['corpus.heldout.jsonl'] = write('corpus.heldout.jsonl', dump(held));
m.files['prior.ids.txt'] = write('prior.ids.txt', prior.join('\n') + '\n');

// The labellers' view: every row, in hash order, with no time and no split.
const view = [...rows].sort((a, b) => (a.id < b.id ? -1 : 1)).map(r => JSON.stringify({ id: r.id, stratum: r.stratum, text: r.text })).join('\n') + '\n';
m.files['label_input.jsonl'] = write('label_input.jsonl', view);

// Readability after redaction: a row with no text left, with under three words left,
// or with more than one removed span per five words left.
const MARK = '[redacted]';
const blank = r => r.text.split('\n').every(l => l.trim() === '' || l.trim() === MARK);
const spans = r => r.text.split(MARK).length - 1;
const wordsLeft = r => r.text.split(MARK).join(' ').split(/\s+/).filter(w => /\p{L}/u.test(w)).length;
const count = rs => rs.reduce((c, r) => ((c[r.stratum] = (c[r.stratum] || 0) + 1), c), {});
const priorSet = new Set(prior);
Object.assign(m, {
  stream_rows: stream,
  stream_rows_by_rule: byRule,
  stream_rows_by_term: byTerm,
  rows_by_term: rows.filter(r => r.term).length,
  rows_blank_whole: rows.filter(blank).length,
  rows_blank_whole_repeats: rows.filter(blank).reduce((n, r) => n + r.repeats, 0),
  term_rows_under_3_words_left: rows.filter(r => r.term && !blank(r) && wordsLeft(r) < 3).length,
  term_rows_dense: rows.filter(r => r.term && !blank(r) && wordsLeft(r) >= 3 && spans(r) * 5 > wordsLeft(r)).length,
  queued_echoes_dropped: queuedEcho,
  repeats_collapsed: repeats,
  tune: count(tune),
  heldout: count(held),
  first_ts: rows[0].ts,
  last_ts: rows[rows.length - 1].ts,
  prior_ids: prior.length,
  prior_in_tune: tune.filter(r => priorSet.has(r.id)).length,
  label_bytes: Buffer.byteLength(view),
  longest_chars: Math.max(...rows.map(r => r.text.length)),
});
const mf = JSON.stringify(m, null, 1) + '\n';
fs.writeFileSync(path.join(out, 'manifest.json'), mf);
console.log(mf);
console.log('manifest sha256', sha(mf));
