// Changed paths → which CI jobs actually exercise them. A coverage map, not a list of
// commands: every ci.yml job runs on every change (there are no per-step lane guards), so
// the question is "does anything in CI read this file", not "what do I run".

// ci.yml job ids, in the workflow's own order. The cargo jobs run over the whole crate, so
// any Rust input, and the workflow itself, is exercised by all of them. `bench` is separate:
// it builds the quarantined bench/ workspace only (D0006), never the RUST_JOBS. checker-laws
// reads every PR diff; checker-selftest runs the skill's fixture suite.
export const RUST_JOBS = ['fmt', 'build-budget', 'clippy', 'test', 'release', 'miri']
export const JOB_IDS = [...RUST_JOBS, 'bench', 'checker-selftest', 'checker-laws']

const RUST = [
  /^src\//, /^tests\//, /^benches\//, /^examples\//, /^build\.rs$/,
  /^Cargo\.(toml|lock)$/, /^rust-toolchain\.toml$/, /^\.cargo\//,
  /^\.github\/workflows\//, /^harness\//,
]
// bench's own sources, its path dependency on harness/, the corpus it runs differential over,
// and its workflow — the only paths the `bench` job reads. Not in RUST_JOBS: that is the
// quarantine (D0006). Once adelie implements Engine (E6), src/ must join this list too.
const BENCH = [/^bench\//, /^harness\//, /^tests\/slt\//, /^\.github\/workflows\/ci\.yml$/]
const SKILL = [/^\.claude\/skills\/adelie\/(lib|bin)\//, /^\.claude\/hooks\//, /^\.github\/workflows\/ci\.yml$/]

export const CI_JOBS = {
  ...Object.fromEntries(RUST_JOBS.map(j => [j, RUST])),
  bench: BENCH,
  'checker-selftest': SKILL,
  'checker-laws': [/./],
}

// Fail open twice over: an empty file list runs everything (a guard that saw nothing must
// not skip), and an unknown job throws (a renamed job fails loud). Unused by ci.yml today;
// kept so a future per-step guard has one tested oracle to call.
export function ciGuard(job, paths) {
  const rules = CI_JOBS[job]
  if (!rules) throw new Error(`ci-guard: unknown job '${job}' — add it to JOB_IDS in lanes.mjs`)
  const files = (paths || []).filter(Boolean)
  const cause = files.find(f => rules.some(re => re.test(f))) || null
  return { job, run: !files.length || !!cause, cause, examined: files.length }
}

// Paths no build or test job reads (checker-laws still diffs them). `local` names the
// check to run before pushing, where one exists.
const UNEXERCISED = [
  {
    kind: 'skill',
    match: SKILL.slice(0, 2),
    local: '.claude/skills/adelie/bin/adelie-check selftest',
    why: "the skill's detectors or hooks — checker-selftest runs the fixture suite",
  },
  {
    kind: 'local-tooling',
    match: [/^justfile$/, /^scripts\//],
    local: 'just ci',
    why: 'CI calls cargo directly and never runs just or scripts/',
  },
  {
    kind: 'prose',
    match: [/\.md$/, /^decisions\//, /^\.beads\//, /^\.claude\//, /^\.agents\//, /^\.codex\//, /^\.cursor\//, /^LICENSE$/, /^\.gitignore$/],
    local: null,
    why: 'docs, decisions, tracker and agent config',
  },
]

// bench/ is kind 'rust' too, just not a RUST_JOBS input: jobsFor still gives it only
// ['bench', 'checker-laws'], since CI_JOBS.bench (not RUST) is what actually matches it.
const isRust = f => RUST.some(re => re.test(f)) || BENCH.some(re => re.test(f))

export function lanes(paths) {
  const files = (paths || []).filter(Boolean)
  const jobsFor = file => JOB_IDS.filter(j => CI_JOBS[j].some(re => re.test(file)))
  const rows = files.map(file => {
    if (isRust(file)) return { file, jobs: jobsFor(file), kind: 'rust', local: null }
    const u = UNEXERCISED.find(r => r.match.some(re => re.test(file)))
    if (u) return { file, jobs: jobsFor(file), kind: u.kind, local: u.local, why: u.why }
    return { file, jobs: jobsFor(file), kind: 'unmapped', local: null }
  })
  return {
    // What the answer is *about*. Without it, an empty file list and a change that
    // genuinely needs no job are the same output, and the first reads as the second.
    examined: files.length,
    rows,
    jobs: JOB_IDS.filter(j => rows.some(r => r.jobs.includes(j))),
    // No build or test job reads these; checker-laws diffing them is not exercising them.
    unexercised: rows.filter(r => r.kind !== 'rust' && r.kind !== 'unmapped').map(r => r.file),
    local: [...new Set(rows.map(r => r.local).filter(Boolean))],
    unmatched: rows.filter(r => r.kind === 'unmapped').map(r => r.file),
  }
}

export function formatLanes(result) {
  const out = [`Examined ${result.examined ?? 0} file(s). Coverage map: which CI jobs exercise each path (every job runs on every change regardless).`]
  if (!result.examined) {
    out.push('Nothing to map — this target has no files, so no job could have applied.')
    out.push('Commit first, or name the files with --paths.')
    return out.join('\n')
  }
  const width = Math.min(48, Math.max(...result.rows.map(r => r.file.length)))
  for (const r of result.rows) {
    const what = r.kind === 'rust'
      ? r.jobs.join(', ')
      : r.kind === 'unmapped'
        ? `UNMAPPED — no rule knows this path; check by hand (${r.jobs.join(', ')})`
        : `no build or test job exercises this; ${r.jobs.join(', ')} only (${r.why})`
    out.push(`  ${r.file.padEnd(width)}  → ${what}`)
  }
  if (result.local.length) {
    out.push('', 'Run before pushing (the build/test jobs do not cover these):')
    for (const l of result.local) out.push(`  ${l}`)
  }
  return out.join('\n')
}
