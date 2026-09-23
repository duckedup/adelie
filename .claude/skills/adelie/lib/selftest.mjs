// Fixture suite for the detectors. Pure — no repository, no git, no network — so a
// broken detector fails here instead of silently passing a real review.

import * as laws from './laws.mjs'
import * as fleet from './fleet.mjs'
import * as pre from './preflight.mjs'
import { lanes, formatLanes, ciGuard, CI_JOBS, JOB_IDS } from './lanes.mjs'
import * as spec from './specdoc.mjs'
import * as guards from './guards.mjs'
import { canonicalId, branchNamesIssue } from './git.mjs'

const cases = []
const test = (name, fn) => cases.push({ name, fn })
const ids = fs => fs.map(f => f.id).sort()

function eq(actual, expected, what) {
  const a = JSON.stringify(actual)
  const e = JSON.stringify(expected)
  if (a !== e) throw new Error(`${what}: expected ${e}, got ${a}`)
}

// ── unsafe ─────────────────────────────────────────────────────────────────

test('unsafe: flagged in any module — nothing is exempt', () => {
  eq(ids(laws.unsafeUse('unsafe { ptr.read() }\n', null, 'src/column/read.rs')), ['unsafe-code'], 'block')
  eq(ids(laws.unsafeUse('pub unsafe fn f() {}\n', null, 'src/lib.rs')), ['unsafe-code'], 'fn')
})

test('unsafe: the word inside a comment is not a use', () => {
  eq(ids(laws.unsafeUse('// this would be unsafe { } to do\n', null, 'src/x.rs')), [], 'findings')
})

test('unsafe: an untouched use is skipped when scoped to added lines', () => {
  const src = 'fn a() {}\nunsafe { x() }\n'
  eq(ids(laws.unsafeUse(src, new Set([1]), 'src/x.rs')), [], 'untouched')
  eq(ids(laws.unsafeUse(src, new Set([2]), 'src/x.rs')), ['unsafe-code'], 'touched')
})

test('unsafe: crate attribute must survive', () => {
  eq(ids(laws.crateAttrWeakened('#![deny(unsafe_code)]\n')), [], 'deny is fine')
  eq(ids(laws.crateAttrWeakened('#![forbid(unsafe_code)]\n')), [], 'forbid is fine')
  eq(ids(laws.crateAttrWeakened('//! adelie\n')), ['unsafe-attr'], 'missing is flagged')
  eq(ids(laws.crateAttrWeakened('#![warn(unsafe_code)]\n')), ['unsafe-attr'], 'weakened is flagged')
})

// ── dependencies (D0004) ───────────────────────────────────────────────────

const manifest = (version, deps) =>
  `[package]\nname = "adelie"\nversion = "${version}"\n\n[dependencies]\n${deps.map(d => `${d} = "1"`).join('\n')}\n`

test('deps: a bundled-C / heavy dep is a hard error', () => {
  for (const d of ['duckdb', 'rocksdb', 'zstd', 'arrow', 'arrow-array', 'datafusion', 'polars', 'openssl', 'aws-lc-rs', 'foo-sys']) {
    eq(ids(laws.newDeps(manifest('0.0.0', []), manifest('0.0.0', [d]))), ['forbidden-dep'], d)
  }
})

test('deps: pure-Rust -sys names and look-alikes are only warnings', () => {
  // `arrowhead` guards the family pattern: `arrow-*` crates, not every name starting "arrow".
  for (const d of ['js-sys', 'windows-sys', 'lz4_flex', 'sqlparser', 'arrowhead']) {
    eq(ids(laws.newDeps(manifest('0.0.0', []), manifest('0.0.0', [d]))), ['new-dep'], d)
  }
})

test('deps: an ordinary new dep is a warning', () => {
  eq(ids(laws.newDeps(manifest('0.0.0', ['serde']), manifest('0.0.0', ['serde', 'bytes']))), ['new-dep'], 'findings')
})

test('deps: a package version bump is not a new dependency', () => {
  eq(ids(laws.newDeps(manifest('0.0.0', ['serde']), manifest('0.1.0', ['serde']))), [], 'findings')
})

test('deps: [dependencies.foo] and build-dependencies are seen', () => {
  const head = '[package]\nversion = "1"\n\n[dependencies.bytes]\nversion = "1"\n\n[build-dependencies]\ncc = "1"\n'
  eq(ids(laws.newDeps('[package]\nversion = "1"\n', head)), ['forbidden-dep', 'new-dep'], 'findings')
})

// ── test placement ─────────────────────────────────────────────────────────

test('test placement: a new top-level tests/*.rs is an error', () => {
  const found = laws.testPlacement(['tests/smoke.rs'])
  eq(ids(found), ['test-placement'], 'findings')
  eq(found[0].severity, 'error', 'severity')
})

test('test placement: a second test binary directory is an error', () => {
  eq(ids(laws.testPlacement(['tests/sql/main.rs'])), ['test-placement'], 'findings')
})

test('test placement: tests/e2e/ main and modules are fine, as is src/', () => {
  eq(ids(laws.testPlacement(['tests/e2e/main.rs', 'tests/e2e/sql.rs', 'tests/e2e/sql/joins.rs', 'src/lib.rs'])), [], 'findings')
})

// ── miri ignores must name a reason ────────────────────────────────────────

test('miri: a bare ignore is an error', () => {
  const src = '#[test]\n#[cfg_attr(miri, ignore)]\nfn sums_match() {\n  assert_eq!(sum(&a), 6);\n}\n'
  const found = laws.miriIgnore(src, null, 'src/agg.rs')
  eq(ids(found), ['miri-ignore'], 'findings')
  eq(found[0].line, 2, 'line')
})

test('miri: a trailing reason is accepted', () => {
  const src = '#[cfg_attr(miri, ignore)] // fsync is unsupported under Miri\n#[test]\nfn flush() {}\n'
  eq(ids(laws.miriIgnore(src, null, 'src/wal.rs')), [], 'findings')
})

test('miri: a reason on the line above counts', () => {
  const src = '#[test]\n// mmap: unsupported syscall under Miri\n#[cfg_attr(miri, ignore)]\nfn maps() {}\n'
  eq(ids(laws.miriIgnore(src, null, 'src/seg.rs')), [], 'findings')
})

test('miri: an empty trailing comment is not a reason', () => {
  eq(ids(laws.miriIgnore('#[cfg_attr(miri, ignore)] //\nfn f() {}\n', null, 'src/x.rs')), ['miri-ignore'], 'findings')
})

test('miri: an untouched bare ignore is skipped when scoped to added lines', () => {
  const src = '#[cfg_attr(miri, ignore)]\nfn f() {}\n'
  eq(ids(laws.miriIgnore(src, new Set([2]), 'src/x.rs')), [], 'untouched')
})

// ── session links ──────────────────────────────────────────────────────────

test('session link: in a commit message is an error', () => {
  const found = laws.sessionLink([{ source: 'commit abc1234', text: '🐧 segment codec\n\nhttps://claude.ai/code/session_01ABC\n' }])
  eq(ids(found), ['session-link'], 'findings')
  eq(found[0].severity, 'error', 'severity')
  eq(found[0].line, 3, 'line within the message')
})

test('session link: in the PR body is an error', () => {
  eq(ids(laws.sessionLink([{ source: 'PR #4 body', text: 'Closes adelie-vnn\nclaude.ai/code/session_x' }])), ['session-link'], 'findings')
})

test('session link: other claude.ai links and clean messages pass', () => {
  const texts = [
    { source: 'commit a', text: '🐧 add codec\n\nCloses adelie-vnn' },
    { source: 'PR #4 body', text: 'See https://claude.ai/code/artifact/abc for the writeup' },
  ]
  eq(ids(laws.sessionLink(texts)), [], 'findings')
  eq(ids(laws.sessionLink([])), [], 'nothing examined')
})

// ── stale tickets ──────────────────────────────────────────────────────────

const TITLES = { 'adelie-vnn': 'a real open issue', 'adelie-k2p': 'another open issue' }

test('stale-ticket: a worked bead with no Closes line warns, naming the full id', () => {
  const found = laws.unclosedTickets(new Set(['adelie-vnn']), new Set(), TITLES)
  eq(ids(found), ['stale-ticket'], 'findings')
  eq(found[0].severity, 'warn', 'severity')
  eq(found[0].detail.includes('bd close adelie-vnn'), true, 'full id in the remedy')
})

test('stale-ticket: a Closes line suppresses it', () => {
  eq(ids(laws.unclosedTickets(new Set(['adelie-vnn']), new Set(['adelie-vnn']), TITLES)), [], 'findings')
})

test('stale-ticket: an acknowledged ref suppresses it without claiming closure', () => {
  eq(ids(laws.unclosedTickets(new Set(['adelie-vnn']), new Set(), TITLES, new Set(['adelie-vnn']))), [], 'findings')
})

test('stale-ticket: acknowledging one bead does not silence another', () => {
  const found = laws.unclosedTickets(new Set(['adelie-vnn', 'adelie-k2p']), new Set(), TITLES, new Set(['adelie-vnn']))
  eq(ids(found), ['stale-ticket'], 'one finding')
  eq(found[0].summary.startsWith('adelie-k2p'), true, 'the un-acknowledged one')
})

test('stale-ticket: a bead with no resolvable open title is not flagged', () => {
  eq(ids(laws.unclosedTickets(new Set(['adelie-skill']), new Set(), {})), [], 'findings')
})

// ── bead ids ───────────────────────────────────────────────────────────────

test('ids: hash-style ids canonicalise to the full id', () => {
  eq(canonicalId('adelie-vnn'), 'adelie-vnn', 'full')
  eq(canonicalId('vnn'), 'adelie-vnn', 'bare hash')
  eq(canonicalId('#adelie-vnn.1'), 'adelie-vnn.1', 'child with #')
  eq(canonicalId('nidus-186'), null, 'foreign prefix')
  eq(canonicalId(''), null, 'empty')
})

test('ids: a branch names a bead by full id or by a leading bare hash', () => {
  eq(branchNamesIssue('austin/adelie-vnn-segment-codec', 'adelie-vnn'), true, 'full id')
  eq(branchNamesIssue('spec/adelie-vnn', 'vnn'), true, 'full id at end')
  eq(branchNamesIssue('austin/vnn-codec', 'adelie-vnn'), true, 'leading bare hash')
  eq(branchNamesIssue('austin/avnnx', 'adelie-vnn'), false, 'substring')
  eq(branchNamesIssue('austin/adelie-vnn.1-child', 'adelie-vnn'), false, 'a child is not its parent')
})

// ── lanes: the CI coverage map ─────────────────────────────────────────────

test('lanes: a Rust source is exercised by every CI job', () => {
  const r = lanes(['src/lib.rs'])
  eq(r.jobs, JOB_IDS, 'all jobs')
  eq(r.rows[0].jobs, ['fmt', 'build-budget', 'clippy', 'test', 'release', 'miri'], 'row')
})

test('lanes: manifests, toolchain, tests/ and workflows hit every job too', () => {
  for (const f of ['Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', 'tests/e2e/main.rs', '.github/workflows/ci.yml']) {
    eq(lanes([f]).jobs, JOB_IDS, f)
  }
})

test('lanes: docs, decisions, .beads and .claude are exercised by no job, honestly said', () => {
  const files = ['README.md', 'AGENTS.md', 'decisions/0004-the-build-budget-is-enforced.md', '.beads/config.yaml', '.claude/skills/adelie/SKILL.md', 'LICENSE']
  const r = lanes(files)
  eq(r.jobs, [], 'no jobs')
  eq(r.unexercised, files, 'all unexercised')
  eq(r.unmatched, [], 'none unmapped')
  eq(/no CI job exercises this/.test(formatLanes(r)), true, 'wording')
})

test('lanes: the skill lib and hooks name the local fixture suite CI never runs', () => {
  const r = lanes(['.claude/skills/adelie/lib/laws.mjs', '.claude/hooks/law-check.mjs'])
  eq(r.jobs, [], 'no CI job')
  eq(r.local, ['.claude/skills/adelie/bin/adelie-check selftest'], 'local check')
})

test('lanes: the justfile is local tooling, not a CI input', () => {
  const r = lanes(['justfile', 'scripts/bd-setup.sh'])
  eq(r.jobs, [], 'no CI job')
  eq(r.local, ['just ci'], 'local check')
})

test('lanes: an unmapped path is reported, not swallowed', () => {
  const r = lanes(['weird/place.txt'])
  eq(r.unmatched, ['weird/place.txt'], 'unmatched')
  eq(/UNMAPPED/.test(formatLanes(r)), true, 'printed')
})

test('lanes: the output is a coverage map, not commands', () => {
  const out = formatLanes(lanes(['src/lib.rs', 'README.md']))
  eq(/Coverage map/.test(out), true, 'labelled')
  eq(/src\/lib\.rs\s+→ fmt, build-budget, clippy, test, release, miri/.test(out), true, `rust row in: ${out}`)
  eq(/cargo |^Run these/m.test(out), false, 'no commands to run')
})

test('lanes: the result states how many files it examined', () => {
  eq(lanes([]).examined, 0, 'nothing examined')
  eq(lanes(['src/lib.rs', 'README.md']).examined, 2, 'counts what it was given')
})

test('lanes: an empty scope does not read like a docs-only change', () => {
  const empty = formatLanes(lanes([]))
  eq(/Examined 0 file/.test(empty), true, `disclosed: ${empty}`)
  eq(empty === formatLanes(lanes(['LICENSE'])), false, 'distinguishable')
})

// ── ci-guard ───────────────────────────────────────────────────────────────

test('ci-guard: the job ids are exactly ci.yml\'s', () => {
  eq(Object.keys(CI_JOBS), ['fmt', 'build-budget', 'clippy', 'test', 'release', 'miri'], 'ids')
})

test('ci-guard: a docs/skill-only change skips every job', () => {
  for (const job of JOB_IDS) eq(ciGuard(job, ['README.md', '.claude/skills/adelie/SKILL.md', 'decisions/0001-x.md']).run, false, job)
})

test('ci-guard: any Rust input runs every job', () => {
  for (const f of ['src/lib.rs', 'Cargo.toml', 'Cargo.lock', 'tests/e2e/main.rs', 'rust-toolchain.toml', '.github/workflows/ci.yml']) {
    for (const job of JOB_IDS) eq(ciGuard(job, ['README.md', f]).run, true, `${job} for ${f}`)
  }
})

test('ci-guard: an empty diff runs everything — a guard that saw nothing must not skip', () => {
  eq(ciGuard('test', []).run, true, 'fail open')
})

test('ci-guard: an unknown job is an error, never a skip', () => {
  let threw = false
  try { ciGuard('test-extended', ['src/lib.rs']) } catch { threw = true }
  eq(threw, true, 'throws')
})

// ── scope honesty ──────────────────────────────────────────────────────────

test('empty scope: an empty changeset is itself a finding', () => {
  eq(ids(laws.emptyScope([], 'range')), ['empty-scope'], 'findings')
})

test('empty scope: a non-empty changeset is silent', () => {
  eq(ids(laws.emptyScope(['src/lib.rs'], 'range')), [], 'findings')
})

test('stale base: a local ref behind its remote is flagged, with both counts', () => {
  const found = laws.staleBase({ ref: 'main', hasRemote: true, behind: 12, examined: 89, examinedFresh: 10 })
  eq(ids(found), ['stale-base'], 'findings')
  eq(found[0].detail.includes('89') && found[0].detail.includes('10'), true, 'both counts named')
})

test('stale base: an up-to-date ref, or one with no remote, is silent', () => {
  eq(ids(laws.staleBase({ ref: 'main', hasRemote: true, behind: 0 })), [], 'fresh')
  eq(ids(laws.staleBase({ ref: 'local-only', hasRemote: false, behind: 0 })), [], 'no remote')
})

test('stale base: equal counts do not claim a difference that is not there', () => {
  const found = laws.staleBase({ ref: 'main', hasRemote: true, behind: 2, examined: 10, examinedFresh: 10 })
  eq(ids(found), ['stale-base'], 'still flagged')
  eq(found[0].detail.includes('examined 10 file(s)'), false, 'no fabricated comparison')
})

// ── skill lane wiring ──────────────────────────────────────────────────────

const ROUTER = '| `fit` | `lanes/fit.md` |\n| `ship` | `lanes/ship.md` |'

test('skill lanes: a routed lane that exists is fine', () => {
  eq(ids(laws.skillLanes(ROUTER, ['fit.md', 'ship.md'])), [], 'findings')
})

test('skill lanes: a routed lane with no file is an error', () => {
  const found = laws.skillLanes(ROUTER, ['fit.md'])
  eq(ids(found), ['skill-lane-missing'], 'findings')
  eq(found[0].file, '.claude/skills/adelie/SKILL.md', 'adelie path')
})

test('skill lanes: a lane file nobody routes to is a warning', () => {
  const found = laws.skillLanes(ROUTER, ['fit.md', 'ship.md', 'fleet.md'])
  eq(ids(found), ['skill-lane-orphan'], 'findings')
  eq(found[0].severity, 'warn', 'severity')
})

// ── context budget and decision pointers ───────────────────────────────────

test('context budget: AGENTS.md at the cap is fine, as is a missing file', () => {
  eq(ids(laws.contextBudget('x\n'.repeat(200))), [], 'at cap')
  eq(ids(laws.contextBudget(null)), [], 'missing')
})

test('context budget: AGENTS.md over the cap is flagged with its line count', () => {
  const found = laws.contextBudget('x\n'.repeat(201))
  eq(ids(found), ['context-budget'], 'findings')
  eq(found[0].file, 'AGENTS.md', 'file')
  eq(found[0].summary.includes('201 lines'), true, 'matches wc -l')
})

test('decision pointers: a cited record that exists is fine', () => {
  eq(ids(laws.decisionPointers([{ file: 'AGENTS.md', text: 'see D0004' }], ['0004-the-build-budget-is-enforced.md'])), [], 'findings')
})

test('decision pointers: a dangling D-number is an error, reported once', () => {
  const found = laws.decisionPointers([{ file: 'AGENTS.md', text: 'see D0099 and again D0099' }], ['0004-x.md'])
  eq(ids(found), ['dangling-decision'], 'one per pointer per file')
  eq(found[0].summary.includes('D0099'), true, 'names the pointer')
})

// ── read guard ─────────────────────────────────────────────────────────────

const blocked = m => (m === null ? [] : ['blocked'])
const SPEC = 'SPEC' + '.md'

test('read guard: an unbounded read of a large SPEC.md is blocked', () => {
  eq(blocked(guards.specRead({ rel: SPEC, lines: 801 })), ['blocked'], 'blocked')
})

test('read guard: a ~400-line SPEC.md reads freely — the threshold is 800', () => {
  eq(guards.GUARD_MIN_LINES, 800, 'threshold')
  eq(blocked(guards.specRead({ rel: SPEC, lines: 395 })), [], '395 lines')
  eq(blocked(guards.specRead({ rel: SPEC, lines: 800 })), [], 'at threshold')
  eq(blocked(guards.specRead({ rel: SPEC })), [], 'unknown size')
})

test('read guard: a bounded read is allowed, but offset alone is not a bound', () => {
  eq(blocked(guards.specRead({ rel: SPEC, lines: 2000, offset: 900, limit: 40 })), [], 'allowed')
  eq(blocked(guards.specRead({ rel: SPEC, lines: 2000, offset: 900 })), ['blocked'], 'offset alone')
})

test('read guard: an unguarded doc and a missing path are allowed', () => {
  eq(blocked(guards.specRead({ rel: 'AGENTS.md', lines: 5000 })), [], 'unguarded')
  eq(blocked(guards.specRead({})), [], 'no path')
  eq(blocked(guards.specRead()), [], 'no args')
})

test('read guard: the message names the tool that replaces the read', () => {
  eq(guards.specRead({ rel: SPEC, lines: 900 }).includes('just spec toc'), true, 'names the tool')
})

// ── spec section addressing ────────────────────────────────────────────────

const DOC = [
  '# Title',                       // 1
  '',                              // 2
  '## 1. First',                   // 3
  'alpha bravo',                   // 4
  '### 1.1 Nested',                // 5
  'charlie',                       // 6
  '## 2. Second',                  // 7
  'delta alpha',                   // 8
  '```',                           // 9
  '## 7. Not a heading',           // 10
  '```',                           // 11
  '### Unnumbered Bit',            // 12
  'echo',                          // 13
]

test('spec: headings carry level, number and span', () => {
  const hs = spec.headings(DOC)
  eq(hs.map(h => h.num), ['1', '1.1', '2', null], 'numbers')
  eq(hs.map(h => h.line), [3, 5, 7, 12], 'lines')
  eq(hs.map(h => h.end), [6, 6, 13, 13], 'ends')
})

test('spec: a heading inside a fence is not a heading', () => {
  eq(spec.headings(DOC).some(h => h.num === '7'), false, 'fenced ## ignored')
})

test('spec: a section spans to the next heading of its own level or higher', () => {
  const h = spec.locate(spec.headings(DOC), '1')
  eq(spec.section(DOC, h), ['## 1. First', 'alpha bravo', '### 1.1 Nested', 'charlie'], 'section 1')
})

test('spec: an exact number is not hijacked by a slug substring', () => {
  eq(spec.locate(spec.headings(DOC), '1').line, 3, 'numeric ref wins')
  eq(spec.locate(spec.headings(DOC), 'unnumbered').line, 12, 'slug substring resolves')
  eq(spec.locate(spec.headings(DOC), '9.9'), null, 'unknown ref is null')
})

test('spec: § and # prefixes are accepted on a ref', () => {
  eq(spec.locate(spec.headings(DOC), '§1.1').line, 5, 'numeric with §')
  eq(spec.locate(spec.headings(DOC), '#unnumbered-bit').line, 12, 'slug with #')
})

test('spec: find needs every word somewhere in the section, not on one line', () => {
  eq(spec.search(DOC, ['alpha', 'charlie']).map(f => f.ref), ['§1'], 'words split across lines')
  eq(spec.search(DOC, ['alpha', 'zulu']), [], 'a missing word matches nothing')
})

test('spec: find reports the innermost matching section, not its parent', () => {
  eq(spec.search(DOC, ['charlie']).map(f => f.ref), ['§1.1'], 'child only')
})

test('spec: find with no words matches nothing rather than everything', () => {
  eq(spec.search(DOC, []), [], 'empty query')
  eq(spec.search(DOC, ['']), [], 'blank word')
})

// ── fleet ──────────────────────────────────────────────────────────────────

const SELF = { dir: '/r/adelie', commonDir: '/r/adelie/.git', remote: 'git@github.com:duckedup/adelie.git', mainSha: 'aaa', login: 'austin' }
const wt = (name, slug, over = {}) => ({
  name, dir: `/r/adelie/.claude/worktrees/${slug}`, isRepo: true, commonDir: '/r/adelie/.git',
  remote: SELF.remote, branch: slug, dirty: false, mainSha: 'aaa', ...over,
})

test('fleet: worktrees off one clone are the clean case', () => {
  eq(ids(fleet.treeFindings([wt('a', 'x'), wt('b', 'y')], SELF)), [], 'findings')
})

test('fleet: two peers in one directory, or in the coordinator tree, is an error', () => {
  eq(ids(fleet.treeFindings([wt('a', 'x'), wt('b', 'x')], SELF)), ['fleet-shared-tree'], 'two peers')
  eq(ids(fleet.treeFindings([wt('a', 'x', { dir: '/r/adelie' })], SELF)), ['fleet-shared-tree'], 'coordinator tree')
})

test('fleet: the coordinator own row is not a collision with itself', () => {
  const me = wt('coordinator', 'x', { dir: '/r/adelie', branch: 'austin/adelie-vnn', self: true })
  eq(ids(fleet.treeFindings([me, wt('a', 'y')], SELF)), [], 'findings')
})

test('fleet: a separate clone warns, a foreign remote errors, spellings match', () => {
  eq(ids(fleet.treeFindings([wt('a', 'x', { dir: '/r/a2', commonDir: '/r/a2/.git' })], SELF)), ['fleet-separate-clone'], 'clone')
  eq(fleet.treeFindings([wt('a', 'x', { commonDir: '/r/o/.git', remote: 'git@github.com:someone/fork.git' })], SELF)
    .some(f => f.id === 'fleet-foreign-remote'), true, 'foreign')
  eq(ids(fleet.treeFindings([wt('a', 'x', { remote: 'https://github.com/duckedup/adelie' })], SELF)), [], 'ssh = https')
})

test('fleet: a peer on main, dirty, behind, or with no tree is reported', () => {
  eq(ids(fleet.treeFindings([wt('a', 'x', { branch: 'main' })], SELF)), ['fleet-on-main'], 'on main')
  eq(ids(fleet.treeFindings([wt('a', 'x', { dirty: true })], SELF)), ['fleet-dirty-tree'], 'dirty')
  eq(ids(fleet.treeFindings([wt('a', 'x', { mainSha: 'bbb' })], SELF)), ['fleet-stale-main'], 'stale')
  eq(ids(fleet.treeFindings([{ name: 'a', dir: null }], SELF)), ['fleet-no-tree'], 'no tree')
  eq(ids(fleet.treeFindings([{ name: 'backlog', dir: null, unassigned: true }], SELF)), [], 'parked queue')
})

const OPEN = { state: 'OPEN', assignees: [], linkedPrs: [] }

test('fleet: an open unclaimed queue is clear', () => {
  eq(ids(fleet.issueFindings([{ name: 'a', queue: ['adelie-vnn'] }], { 'adelie-vnn': OPEN }, { login: 'austin' })), [], 'findings')
})

test('fleet: the same bead in two queues is an error, subject is the full id', () => {
  const found = fleet.issueFindings([{ name: 'a', queue: ['adelie-vnn'] }, { name: 'b', queue: ['adelie-vnn'] }], { 'adelie-vnn': OPEN })
  eq(ids(found), ['fleet-double-assigned'], 'findings')
  eq(found[0].subject, 'adelie-vnn', 'subject')
})

test('fleet: a closed or missing bead is an error', () => {
  eq(ids(fleet.issueFindings([{ name: 'a', queue: ['adelie-vnn'] }], { 'adelie-vnn': { ...OPEN, state: 'CLOSED' } })), ['fleet-issue-closed'], 'closed')
  eq(ids(fleet.issueFindings([{ name: 'a', queue: ['adelie-vnn'] }], {})), ['fleet-issue-missing'], 'missing')
})

test('fleet: a bead assigned elsewhere warns, but not against yourself', () => {
  eq(ids(fleet.issueFindings([{ name: 'a', queue: ['adelie-vnn'] }], { 'adelie-vnn': { ...OPEN, assignees: ['someone'] } }, { login: 'austin' })), ['fleet-issue-taken'], 'taken')
  eq(ids(fleet.issueFindings([{ name: 'a', queue: ['adelie-vnn'] }], { 'adelie-vnn': { ...OPEN, assignees: ['austin'] } }, { login: 'austin' })), [], 'mine')
})

test('fleet: an open PR closing the bead warns; a merged one does not', () => {
  eq(ids(fleet.issueFindings([{ name: 'a', queue: ['adelie-vnn'] }], { 'adelie-vnn': { ...OPEN, linkedPrs: [{ number: 5, state: 'OPEN' }] } })), ['fleet-issue-has-pr'], 'open')
  eq(ids(fleet.issueFindings([{ name: 'a', queue: ['adelie-vnn'] }], { 'adelie-vnn': { ...OPEN, linkedPrs: [{ number: 5, state: 'MERGED' }] } })), [], 'merged')
})

test('fleet: two peers claiming one file is flagged; one peer across its own beads is not', () => {
  const two = [{ name: 'a', surface: { 'adelie-vnn': ['src/lib.rs'] } }, { name: 'b', surface: { 'adelie-k2p': ['src/lib.rs'] } }]
  eq(ids(fleet.overlapFindings(two)), ['fleet-file-overlap'], 'overlap')
  eq(ids(fleet.overlapFindings([{ name: 'a', surface: { 'adelie-vnn': ['src/lib.rs'], 'adelie-k2p': ['src/lib.rs'] } }])), [], 'one peer')
})

const MAIN = { path: '/r/adelie', branch: 'main', isMain: true, dirty: false, hasCommits: false }

test('fleet: an agent worktree nobody claims is an orphan; a claimed one is not', () => {
  const agent = { path: '/r/adelie/.claude/worktrees/agent-a0c1', branch: 'worktree-agent-a0c1', isMain: false, dirty: false, hasCommits: true }
  const found = fleet.orphanFindings([MAIN, agent], [], SELF)
  eq(ids(found), ['fleet-orphan-agent-worktree'], 'orphan')
  eq(/--force/.test(found[0].detail), true, 'carries commits: force')
  eq(ids(fleet.orphanFindings([MAIN, agent], [{ name: 'a', dir: agent.path }], SELF)), [], 'claimed')
})

test('fleet: rehydrate derives shipped / in-review / in-flight / queued from bead ids', () => {
  const peers = [{ name: 'p', queue: ['adelie-aaa', 'adelie-bbb', 'adelie-ccc', 'adelie-ddd'] }]
  const issues = {
    'adelie-aaa': { state: 'CLOSED', linkedPrs: [{ number: 1, state: 'MERGED' }] },
    'adelie-bbb': { state: 'OPEN', linkedPrs: [{ number: 2, state: 'OPEN' }] },
    'adelie-ccc': { state: 'OPEN', linkedPrs: [] },
    'adelie-ddd': { state: 'OPEN', linkedPrs: [] },
  }
  const trees = [MAIN, { path: '/r/adelie/.claude/worktrees/x', branch: 'austin/adelie-ccc-sweep', isMain: false }]
  const rows = fleet.rehydrate(peers, issues, trees)
  eq(rows.map(r => r.state), ['shipped', 'in-review', 'in-flight', 'queued'], 'states')
})

test('fleet: a pushed branch counts as in-flight; a look-alike id does not', () => {
  const issues = { 'adelie-vnn': { state: 'OPEN', linkedPrs: [] } }
  eq(fleet.rehydrate([{ name: 'p', queue: ['adelie-vnn'] }], issues, [MAIN], ['origin/spec/adelie-vnn'])[0].state, 'in-flight', 'pushed')
  eq(fleet.rehydrate([{ name: 'p', queue: ['adelie-vnn'] }], issues, [MAIN], ['origin/austin/adelie-vnnx'])[0].state, 'queued', 'look-alike')
})

test('fleet: a bundled sibling inherits its lead\'s state', () => {
  const peers = [{ name: 'p', queue: ['adelie-aaa', 'adelie-bbb'], bundles: [['adelie-aaa', 'adelie-bbb']] }]
  const issues = { 'adelie-aaa': { state: 'OPEN', linkedPrs: [] }, 'adelie-bbb': { state: 'OPEN', linkedPrs: [] } }
  const rows = fleet.rehydrate(peers, issues, [MAIN], ['origin/austin/adelie-aaa-x'])
  eq(rows.map(r => r.state), ['in-flight', 'in-flight'], 'both')
  eq(rows[1].via, 'adelie-aaa', 'names its lead')
  eq(fleet.formatRehydrate(rows).includes('bundled with adelie-aaa'), true, 'formatted')
})

// ── preflight ──────────────────────────────────────────────────────────────

const clean = { fetched: true, branch: 'austin/adelie-vnn', onMain: false, dirty: false, behind: 0 }

test('preflight: a fresh branch off a fetched main is clear', () => {
  eq(ids(pre.preflight(clean)), [], 'findings')
})

test('preflight: --no-fetch is itself the finding', () => {
  eq(ids(pre.preflight({ ...clean, fetched: false })), ['preflight-no-fetch'], 'findings')
})

test('preflight: behind origin/main blocks, and names the count', () => {
  const found = pre.preflight({ ...clean, behind: 12 })
  eq(ids(found), ['preflight-stale-base'], 'findings')
  eq(found[0].summary.includes('12'), true, 'count')
})

test('preflight: on main reports that instead of staleness', () => {
  eq(ids(pre.preflight({ ...clean, onMain: true, branch: 'main', behind: 3 })), ['preflight-on-main'], 'one finding')
})

test('preflight: a dirty tree warns but does not block', () => {
  eq(pre.preflight({ ...clean, dirty: true }).map(f => f.severity), ['warn'], 'severity')
})

test('preflight: unpushed commits on local main warn; diverged reports both', () => {
  eq(ids(pre.preflight({ ...clean, mainAhead: 2 })), ['preflight-unpushed-main'], 'ahead')
  eq(ids(pre.preflight({ ...clean, behind: 3, mainAhead: 2 })), ['preflight-stale-base', 'preflight-unpushed-main'], 'diverged')
})

test('preflight: a closed bead shipped by a merged PR blocks on both counts, subject is the full id', () => {
  const issue = { id: 'adelie-vnn', state: 'CLOSED', assignees: [], linkedPrs: [{ number: 3, state: 'MERGED' }] }
  const found = pre.preflight({ ...clean, issue })
  eq(ids(found), ['preflight-ticket-closed', 'preflight-ticket-shipped'], 'findings')
  eq(found[0].subject, 'adelie-vnn', 'subject')
})

test('preflight: an open PR, a deferred bead, and someone else\'s claim are all reported', () => {
  eq(ids(pre.preflight({ ...clean, issue: { id: 'adelie-vnn', state: 'OPEN', assignees: [], linkedPrs: [{ number: 4, state: 'OPEN' }] } })), ['preflight-ticket-in-pr'], 'in pr')
  eq(ids(pre.preflight({ ...clean, issue: { id: 'adelie-vnn', state: 'DEFERRED', assignees: [], linkedPrs: [] } })), ['preflight-ticket-deferred'], 'deferred')
  const mine = { id: 'adelie-vnn', state: 'OPEN', assignees: ['Austin'], linkedPrs: [] }
  eq(ids(pre.preflight({ ...clean, issue: mine, me: ['Austin'] })), [], 'mine')
  eq(ids(pre.preflight({ ...clean, issue: mine, me: ['someone'] })), ['preflight-ticket-taken'], 'theirs')
})

test('preflight: a bead bd cannot resolve warns rather than being invented', () => {
  eq(ids(pre.preflight({ ...clean, issue: { id: 'adelie-zzz', unknown: true } })), ['preflight-ticket-unknown'], 'findings')
})

test('preflight: a foreign remote branch for the bead warns; our own does not', () => {
  const issue = { id: 'adelie-vnn', state: 'OPEN', assignees: [], linkedPrs: [] }
  eq(ids(pre.preflight({ ...clean, issue, issueBranches: ['origin/austin/adelie-vnn'] })), [], 'ours')
  eq(ids(pre.preflight({ ...clean, issue, issueBranches: ['origin/spec/adelie-vnn'] })), ['preflight-branch-exists'], 'foreign')
})

test('preflight: every bead in a bundle is checked, not just the first', () => {
  const ok = { id: 'adelie-aaa', state: 'OPEN', assignees: [], linkedPrs: [] }
  const shipped = { id: 'adelie-ccc', state: 'CLOSED', assignees: [], linkedPrs: [] }
  eq(ids(pre.preflight({ ...clean, issues: [{ issue: ok }, { issue: shipped }] })), ['preflight-ticket-closed'], 'the second blocks')
})

test('preflight: no version line at 0.0.0 with no tags — no release process yet', () => {
  eq(pre.versionLine({ mainVersion: '0.0.0', nextVersion: null, tagCount: 0 }), null, 'silent')
  eq(/version/.test(pre.formatPreflight([], { branch: 'b', fetched: true, mainVersion: '0.0.0', tagCount: 0 })), false, 'not printed')
})

test('preflight: a version line once tags exist, with the next free version', () => {
  eq(pre.nextFreeVersion('0.2.0', [{ ref: 'origin/a', version: '0.3.0' }], new Set(['v0.2.0'])), '0.4.0', 'past the claim')
  eq(pre.nextFreeVersion('0.9.0', [], new Set(['v0.10.0'])), '0.11.0', 'numeric, past a tag')
  eq(/next free version to claim: 0\.4\.0/.test(pre.versionLine({ mainVersion: '0.2.0', nextVersion: '0.4.0', tagCount: 1 })), true, 'printed')
})

export function selftest({ json = false } = {}) {
  const failures = []
  for (const c of cases) {
    try { c.fn() } catch (e) { failures.push({ name: c.name, error: e.message }) }
  }
  if (json) {
    console.log(JSON.stringify({ total: cases.length, failed: failures.length, failures }, null, 2))
  } else {
    for (const f of failures) console.log(`✗ ${f.name}\n    ${f.error}`)
    console.log(`${cases.length - failures.length}/${cases.length} detector tests passed`)
  }
  return failures.length ? 1 : 0
}
