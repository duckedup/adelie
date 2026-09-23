// Entry point for bin/adelie-check. Wires the pure detectors in laws.mjs and the
// lane map in lanes.mjs to real git/gh IO.

import { lanes, formatLanes, ciGuard, JOB_IDS } from './lanes.mjs'
import * as laws from './laws.mjs'
import * as fleet from './fleet.mjs'
import * as pre from './preflight.mjs'
import * as git from './git.mjs'
import { selftest } from './selftest.mjs'
import { readFileSync, existsSync, readdirSync } from 'node:fs'

const argv = process.argv.slice(2)
const cmd = argv[0]
const flag = name => {
  const i = argv.indexOf(`--${name}`)
  return i === -1 ? null : (argv[i + 1] && !argv[i + 1].startsWith('--') ? argv[i + 1] : true)
}
const asJson = argv.includes('--json')

function target() {
  return git.resolveTarget({
    base: flag('base') === true ? null : flag('base'),
    head: flag('head') === true ? null : flag('head'),
    pr: flag('pr') === true ? null : flag('pr'),
    path: flag('path') === true ? null : flag('path'),
  })
}

function runLanes() {
  const explicit = flag('paths')
  const files = typeof explicit === 'string' ? explicit.split(',').map(s => s.trim()) : git.changedFiles(target())
  const result = lanes(files)
  if (asJson) { console.log(JSON.stringify({ files, ...result }, null, 2)); return 0 }
  console.log(formatLanes(result))
  return 0
}

// Stdout is the verdict alone (`run` | `skip`) so a CI step can test it; the
// reason goes to stderr. Exit 0 either way — only an unknown job is a failure.
function runCiGuard() {
  const job = argv[1]
  const explicit = flag('paths')
  const files = typeof explicit === 'string' ? explicit.split(',').map(s => s.trim()) : git.changedFiles(target())
  let r
  try { r = ciGuard(job, files) } catch (e) { console.error(String(e.message || e)); return 1 }
  if (asJson) { console.log(JSON.stringify(r)); return 0 }
  console.error(r.run
    ? (r.cause ? `run: ${r.cause} exercises ${job}` : 'run: no files examined, so everything runs')
    : `skip: none of ${r.examined} changed file(s) exercise ${job}`)
  console.log(r.run ? 'run' : 'skip')
  return 0
}

const RS = f => f.endsWith('.rs')

function runLaws() {
  const t = target()
  const changed = git.changedFiles(t)
  // A --base over a stale local ref examines a range nobody meant and reports it as
  // thoroughness (found in nidus, nidus-qko). The two file counts are the tell.
  const drift = t.kind === 'range' ? git.refDrift(flag('base') === true ? null : flag('base') || 'main') : { behind: 0 }
  const driftFindings = drift.behind > 0
    ? laws.staleBase({ ...drift, examined: changed.length, examinedFresh: git.changedFiles(git.resolveTarget({ base: `origin/${drift.ref}`, head: flag('head') === true ? null : flag('head') })).length })
    : []
  const added = git.addedFiles(t)
  const addedLines = git.addedLineMap(t)
  const findings = []
  findings.push(...driftFindings)
  findings.push(...laws.emptyScope(changed, t.kind))

  for (const f of changed.filter(RS)) {
    const text = git.readAt(t, f)
    if (text == null) continue
    const lines = addedLines.get(f) || null
    findings.push(...laws.unsafeUse(text, lines, f))
    findings.push(...laws.miriIgnore(text, lines, f))
  }

  const libText = git.readAt(t, 'src/lib.rs')
  if (libText) findings.push(...laws.crateAttrWeakened(libText))

  // A --path sweep has no base commit, so the law that compares two revisions does not apply.
  if (t.base && changed.includes('Cargo.toml')) {
    findings.push(...laws.newDeps(git.readBase(t, 'Cargo.toml') || '', git.readAt(t, 'Cargo.toml') || ''))
  }

  findings.push(...laws.testPlacement(added))
  findings.push(...laws.sessionLink(git.messageTexts(t)))

  // The working tree, not a revision: these are broken now, whatever the diff.
  const repo = new URL('../../../../', import.meta.url).pathname
  const read = f => (existsSync(`${repo}${f}`) ? readFileSync(`${repo}${f}`, 'utf8') : null)
  const list = (d, ext) => (existsSync(`${repo}${d}`) ? readdirSync(`${repo}${d}`).filter(f => f.endsWith(ext)) : [])
  const agentsMd = read('AGENTS.md')
  findings.push(...laws.contextBudget(agentsMd))
  const decisions = list('decisions', '.md').filter(f => f !== 'README.md')
  const skillDir = new URL('..', import.meta.url).pathname
  const laneFiles = existsSync(`${skillDir}lanes`) ? readdirSync(`${skillDir}lanes`).filter(f => f.endsWith('.md')) : []
  const citing = [
    'AGENTS.md', 'Cargo.toml', 'justfile',
    ...list('decisions', '.md').map(f => `decisions/${f}`),
    ...list('.github/workflows', '.yml').map(f => `.github/workflows/${f}`),
    '.claude/skills/adelie/SKILL.md',
    ...laneFiles.map(f => `.claude/skills/adelie/lanes/${f}`),
  ].map(file => ({ file, text: read(file) })).filter(x => x.text != null)
  findings.push(...laws.decisionPointers(citing, decisions))

  if (existsSync(`${skillDir}SKILL.md`)) {
    findings.push(...laws.skillLanes(readFileSync(`${skillDir}SKILL.md`, 'utf8'), laneFiles))
  }
  const mentioned = git.mentionedIssues(t)
  findings.push(...laws.unclosedTickets(
    mentioned, git.closingIssues(t), git.issueTitles(mentioned), git.acknowledgedIssues(t),
  ))

  const errors = findings.filter(f => f.severity === 'error')
  if (asJson) {
    console.log(JSON.stringify({ target: { kind: t.kind, base: t.base, head: t.head }, changed, findings }, null, 2))
  } else {
    // Printed unconditionally: a run over nothing must never read like a clean one.
    const stale = driftFindings.length ? ` — WARNING: base ${drift.ref} is ${drift.behind} commit(s) behind origin/${drift.ref}` : ''
    console.log(`Examined ${changed.length} changed file(s) (${t.kind})${stale}.`)
    if (!findings.length) console.log('No law violations.')
    for (const f of findings) {
      console.log(`${f.severity === 'error' ? '✗' : '!'} [${f.id}] ${f.file}:${f.line} — ${f.summary}\n    ${f.detail}`)
    }
    if (findings.length) console.log(`\n${errors.length} error(s), ${findings.length - errors.length} warning(s)`)
  }
  return errors.length || (argv.includes('--strict') && findings.length) ? 1 : 0
}

// The plan is the coordinator's only un-derivable state, so it lives on disk rather
// than in a context that cannot clear itself.
const PLAN = '.claude/fleet-plan.json'

const mainVersion_ = () => (git.sh('git show origin/main:Cargo.toml', { allowFail: true }).match(/^version\s*=\s*"([^"]+)"/m) || [])[1] || null
const canon = id => git.canonicalId(id) || String(id)

function runFleet() {
  const explicit = flag('plan')
  const planPath = typeof explicit === 'string' ? explicit : PLAN
  if (!existsSync(planPath)) {
    console.error(`fleet: no plan at ${planPath}. Write one, or pass --plan <file.json>.`)
    return 1
  }
  const plan = JSON.parse(readFileSync(planPath, 'utf8'))
  const self = git.selfFacts()

  // Queue entries may be written `vnn` or `adelie-vnn`; everything downstream keys on the full id.
  const peers = (plan.peers || []).map(p => ({
    ...p, ...(p.dir ? git.treeFacts(p.dir) : {}), name: p.name,
    queue: (p.queue || []).map(canon),
    bundles: (p.bundles || []).map(b => b.map(canon)),
    surface: Object.fromEntries(Object.entries(p.surface || {}).map(([k, v]) => [canon(k), v])),
  }))
  const queued = [...new Set(peers.flatMap(p => p.queue))]
  const issues = queued.length ? git.issueFacts(queued) : {}
  const trees = git.worktrees()

  const findings = [
    ...fleet.treeFindings(peers, self),
    ...fleet.issueFindings(peers, issues, { login: self.login }),
    ...fleet.overlapFindings(peers),
    ...fleet.orphanFindings(trees, peers, self),
  ]
  const state = fleet.rehydrate(peers, issues, trees, git.remoteBranches())

  if (asJson) console.log(JSON.stringify({ plan: planPath, self, peers, issues, state, findings }, null, 2))
  else if (argv.includes('--status')) console.log(fleet.formatRehydrate(state))
  else console.log(`${fleet.formatRehydrate(state)}\n\n${fleet.formatFleet(findings)}`)
  return findings.some(f => f.severity === 'error') || (argv.includes('--strict') && findings.length) ? 1 : 0
}


function runPreflight() {
  const noFetch = argv.includes('--no-fetch')
  const fetched = noFetch ? false : git.fetchOrigin()
  const self = git.treeFacts(process.cwd())
  const behind = git.behindMain()
  const mainAhead = git.refDrift('main').ahead
  const raw = flag('issue')
  // A comma-separated list, because several tickets land as one branch and one PR: each
  // still needs its own closed/claimed/already-shipped check, and running preflight once
  // per ticket means N fetches and N chances to skip the last one.
  const ids = typeof raw === 'string'
    ? raw.split(',').map(s => s.trim()).filter(Boolean).map(canon)
    : []

  const facts = ids.length ? git.issueFacts(ids) : {}
  const tickets = ids.map(id => ({
    issue: facts[id] || { id, unknown: true },
    issueBranches: git.branchesForIssue(id),
  }))
  const issue = tickets.length ? tickets[0].issue : null
  const issueBranches = tickets.length ? tickets[0].issueBranches : []

  // Versions only mean something once a v* tag exists; until then skip the branch walk too.
  const mainVersion = mainVersion_()
  const released = git.releasedTags()
  const claimed = released.size ? git.inflightVersions() : []
  const nextVersion = released.size ? pre.nextFreeVersion(mainVersion, claimed, released) : null

  const findings = pre.preflight({
    fetched, branch: self.branch, onMain: self.branch === 'main',
    dirty: self.dirty, behind, mainAhead, issue, issueBranches,
    issues: tickets,
    me: git.identities(),
  })
  const info = { branch: self.branch, behind, mainAhead, mainVersion, nextVersion, tagCount: released.size, fetched }

  if (asJson) console.log(JSON.stringify({ info, claimed, issue, issueBranches, tickets, findings }, null, 2))
  else console.log(pre.formatPreflight(findings, info))
  return findings.some(f => f.severity === 'error') || (argv.includes('--strict') && findings.length) ? 1 : 0
}

const USAGE = `adelie-check — deterministic checks for this repo's laws and CI coverage

  adelie-check lanes  [--base <ref>] [--pr <n>] [--paths a,b] [--json]
      Coverage map: which CI jobs (${JOB_IDS.join(', ')}) exercise each changed
      path. Every job runs on every change; this says which ones actually read it,
      and names the local check for paths CI never reads (the skill's own lib).

  adelie-check ci-guard <job> [--paths a,b] [--json]
      Would this CI job's work be relevant to these files? Prints \`run\` or \`skip\`
      on stdout (reason on stderr). ci.yml has no per-step guards today; this is
      the oracle one would call. Job ids live in JOB_IDS in lanes.mjs.

  adelie-check laws   [--base <ref>] [--pr <n>] [--path <p>] [--json] [--strict]
      AGENTS.md / decisions / ci.yml rules as detectors: unsafe code and the crate
      attribute, heavy or -sys deps (D0004), one e2e test binary, Miri ignores that
      name no reason, session links in commit messages or the PR body, beads worked
      but not closed, skill lane wiring, AGENTS.md size, dangling D#### pointers.

  adelie-check fleet  [--plan <file.json>] [--status] [--json] [--strict]
      Is this dispatch safe? Shared working trees, foreign remotes, dirty or stale
      peer clones, beads that are closed/taken/already-PR'd or queued twice, files
      two peers both claim, and worktrees left behind by finished agents.
      Defaults to .claude/fleet-plan.json, the coordinator's durable state; the
      rest is derived, so a cleared session rehydrates with one run. --status
      prints just that. The plan is
      {"peers":[{"name":…,"dir":…,"self":true?,"queue":["adelie-vnn"],"surface":{"adelie-vnn":["path"]}}]}.

  adelie-check preflight [--issue <id>[,<id>…]] [--no-fetch] [--json] [--strict]
      Run this FIRST. Fetches origin, then reports whether this tree is fit to
      reason from: behind origin/main, on main, dirty, and — with --issue (a bead
      id: adelie-vnn or vnn) — whether it is closed, already carried by a merged or
      open PR, assigned to someone else, or already has a remote branch.

  adelie-check selftest
      Run the fixture suite for the detectors.

With no --base/--pr, laws and lanes compare the working tree against HEAD.`

const exit = (() => {
  switch (cmd) {
    case 'lanes': return runLanes()
    case 'ci-guard': return runCiGuard()
    case 'laws': return runLaws()
    case 'fleet': return runFleet()
    case 'preflight': return runPreflight()
    case 'selftest': return selftest({ json: asJson })
    default: console.log(USAGE); return cmd ? 1 : 0
  }
})()

process.exit(exit)
