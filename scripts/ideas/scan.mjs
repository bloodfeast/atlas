// Prove no denylist term survives in a frozen corpus: every string value of every row
// goes to `redact_filter --scan`, which prints counts only and exits nonzero on a hit.
//
//   node scan.mjs redact_filter.exe FILE.jsonl...
import fs from 'fs';
import { spawnSync } from 'child_process';

const [filter, ...files] = process.argv.slice(2);
const esc = s => s.replace(/\\/g, '\\\\').replace(/\r/g, '\\r').replace(/\n/g, '\\n');
let failed = false;
for (const f of files) {
  const values = fs.readFileSync(f, 'utf8').trim().split('\n')
    .flatMap(l => Object.values(JSON.parse(l)).filter(v => typeof v === 'string'));
  const run = spawnSync(filter, ['--scan'], { input: values.map(esc).join('\n') + '\n', encoding: 'utf8', maxBuffer: 1 << 30 });
  console.log(f.split(/[\\/]/).pop(), run.stdout.trim());
  if (run.status !== 0) failed = true;
}
process.exit(failed ? 1 : 0);
