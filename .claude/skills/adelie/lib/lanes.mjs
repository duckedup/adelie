// Changed paths → which CI jobs actually exercise them. A coverage map, not a list of
// commands: every ci.yml job runs on every change (there are no per-step lane guards), so
// the question this answers is "does anything in CI read this file", not "what do I run".

// ci.yml job ids. Each runs cargo over the whole crate, so any Rust input, and the
// workflow itself, is exercised by all of them.
export const JOB_IDS = ['fmt', 'build-budget', 'clippy', 'test', 'release', 'miri']

const RUST = [
  /^src\//, /^tests\//, /^benches\//, /^examples\//, /^build\.rs$/,
  /^Cargo\.(toml|lock)$/, /^rust-toolchain\.toml$/, /^\.cargo\//,
  /^\.github\/workflows\//,
]

export const CI_JOBS = Object.fromEntries(JOB_IDS.map(j => [j, RUST]))

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

// Paths CI never reads. `local` names the one check that does cover them, where one exists.
const UNEXERCISED = [
  {
    kind: 'skill',
    match: [/^\.claude\/skills\/adelie\/(lib|bin)\//, /^\.claude\/hooks\//],
    local: '.claude/skills/adelie/bin/adelie-check selftest',
    why: "the skill's detectors or hooks — only the fixture suite proves they still fire",
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
    why: 'docs, decisions, tracker and agent config: nothing in CI reads them',
  },
]

const isRust = f => RUST.some(re => re.test(f))

export function lanes(paths) {
  const files = (paths || []).filter(Boolean)
  const rows = files.map(file => {
    if (isRust(file)) return { file, jobs: JOB_IDS.filter(j => CI_JOBS[j].some(re => re.test(file))), kind: 'rust', local: null }
    const u = UNEXERCISED.find(r => r.match.some(re => re.test(file)))
    if (u) return { file, jobs: [], kind: u.kind, local: u.local, why: u.why }
    return { file, jobs: [], kind: 'unmapped', local: null }
  })
  return {
    // What the answer is *about*. Without it, an empty file list and a change that
    // genuinely needs no job are the same output, and the first reads as the second.
    examined: files.length,
    rows,
    jobs: JOB_IDS.filter(j => rows.some(r => r.jobs.includes(j))),
    unexercised: rows.filter(r => !r.jobs.length && r.kind !== 'unmapped').map(r => r.file),
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
    const what = r.jobs.length
      ? r.jobs.join(', ')
      : r.kind === 'unmapped'
        ? 'UNMAPPED — no rule knows this path; check by hand'
        : `no CI job exercises this (${r.why})`
    out.push(`  ${r.file.padEnd(width)}  → ${what}`)
  }
  if (result.local.length) {
    out.push('', 'Not covered by CI; the local check that does cover it:')
    for (const l of result.local) out.push(`  ${l}`)
  }
  return out.join('\n')
}
