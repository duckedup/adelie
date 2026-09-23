// Deterministic detectors for adelie's repo laws — the rules in AGENTS.md, decisions/ and
// ci.yml that a reviewer restates as prose and an LLM reviewer then forgets. Every check
// here is a pure function over text so lib/selftest.mjs can drive it from fixtures.

const finding = (id, severity, file, line, summary, detail) => ({ id, severity, file, line, summary, detail })

const COMMENT = /^\s*\/\/(\/|!)?/

// ── unsafe code ─────────────────────────────────────────────────────────────
// src/lib.rs is `#![deny(unsafe_code)]` with no sanctioned opt-out yet, so every use is new.

const UNSAFE_USE = /\bunsafe\s*(\{|fn\b|impl\b|trait\b)/

export function unsafeUse(text, addedLines, file) {
  const lines = text.split('\n')
  const out = []
  for (let i = 0; i < lines.length; i++) {
    const raw = lines[i]
    if (COMMENT.test(raw) || !UNSAFE_USE.test(raw)) continue
    if (addedLines && !addedLines.has(i + 1)) continue
    out.push(finding('unsafe-code', 'error', file, i + 1,
      'new `unsafe` code',
      'AGENTS.md: the crate is #![deny(unsafe_code)] and nothing is exempt. An opt-out is a design change: issue and decision record first.'))
  }
  return out
}

export function crateAttrWeakened(libText) {
  if (/#!\[(deny|forbid)\(unsafe_code\)\]/.test(libText)) return []
  return [finding('unsafe-attr', 'error', 'src/lib.rs', 1,
    'the crate-level unsafe_code attribute is gone',
    'AGENTS.md: src/lib.rs keeps #![deny(unsafe_code)] (or forbid).')]
}

// ── Scope honesty ───────────────────────────────────────────────────────────
// `adelie-check laws --base <local ref>` over a ref behind its remote counterpart examines
// a range nobody meant, and reports it as thoroughness (found in nidus, nidus-qko). The two
// file counts are the tell that distinguishes the two.
export function staleBase({ ref, hasRemote, behind = 0, examined = null, examinedFresh = null } = {}) {
  if (!ref || !hasRemote || behind <= 0) return []
  const counts = examined !== null && examinedFresh !== null && examined !== examinedFresh
    ? ` This run examined ${examined} file(s); against origin/${ref} it is ${examinedFresh}.`
    : ''
  return [finding('stale-base', 'warn', ref, 1,
    `local ${ref} is ${behind} commit(s) behind origin/${ref}, so this range is not the one you meant`,
    `A green result over the wrong range says nothing about your work.${counts} Re-run with --base origin/${ref}.`)]
}

// A run that examined nothing must not read as a pass. A finding, not a console line, so it
// survives --json and cannot be hidden by another finding (found in nidus, #173).
export function emptyScope(changed, kind = 'range') {
  if ((changed || []).length) return []
  return [finding('empty-scope', 'warn', 'scope', 1,
    `no files examined — this ${kind} contains no changes, so no law could have failed`,
    'A clean result here says nothing about your work. Commit first, or widen the target with --base/--pr/--path.')]
}

// ── Version bump (D0005) ────────────────────────────────────────────────────
// release.yml publishes only when Cargo.toml's version has no `v<version>` tag, so a
// behavioural PR that does not bump ships nothing. 0.0.0 is a placeholder that never releases.

export const BEHAVIOURAL = [/^src\//, /^Cargo\.toml$/]

export const versionOf = t => (String(t || '').match(/^version\s*=\s*"([^"]+)"/m) || [])[1] || null

const cmpVersion = (a, b) => {
  const pa = String(a).split('.').map(Number)
  const pb = String(b).split('.').map(Number)
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const d = (pa[i] || 0) - (pb[i] || 0)
    if (d) return d
  }
  return 0
}

export function versionBump(baseCargo, headCargo, changed) {
  const touched = (changed || []).filter(f => BEHAVIOURAL.some(re => re.test(f)))
  if (!touched.length) return []
  const base = versionOf(baseCargo)
  const head = versionOf(headCargo)
  if (base && head && base !== head) return []
  return [finding('version-bump', 'error', 'Cargo.toml', 1,
    `version stayed at ${head || '?'} despite behavioural changes`,
    `D0005: bump the version in every PR with a user-visible or behavioural change; release.yml publishes only an untagged version, so an un-bumped PR ships NOTHING. Changed: ${touched.slice(0, 6).join(', ')}${touched.length > 6 ? '…' : ''}`)]
}

// versionBump compares merge base to head, so it cannot see a branch claiming a version
// origin/main already has or has passed (found in nidus, #173 / nidus-7nk). Gated on the
// version having *changed*, so a dependency-only edit at the same version stays clean.
export function versionBackwards(baseCargo, headCargo, originCargo, changed) {
  if (!(changed || []).includes('Cargo.toml')) return []
  const base = versionOf(baseCargo)
  const head = versionOf(headCargo)
  const origin = versionOf(originCargo)
  if (!head || !origin || base === head) return []
  if (cmpVersion(head, origin) > 0) return []
  const how = cmpVersion(head, origin) === 0 ? `is already origin/main's` : `is behind origin/main's`
  return [finding('version-backwards', 'error', 'Cargo.toml', 1,
    `version ${head} ${how} ${origin}`,
    'D0005: release.yml only publishes a version it has not tagged, so merging this ships NOTHING, silently. Pick a version strictly above origin/main.')]
}

// The gap versionBackwards cannot see (found in nidus, nidus-zin): a version above
// origin/main whose tag already exists. No readable tag list means no finding at all, so a
// fresh or offline clone never sees a false one; gated on Cargo.toml itself changing.
export function versionAlreadyTagged(baseCargo, headCargo, tags, changed) {
  if (!(changed || []).includes('Cargo.toml')) return []
  const base = versionOf(baseCargo)
  const head = versionOf(headCargo)
  if (!head || base === head) return []
  if (!tags || typeof tags.has !== 'function' || tags.size === 0) return []
  if (!tags.has(`v${head}`)) return []
  return [finding('version-already-tagged', 'error', 'Cargo.toml', 1,
    `version ${head} is already released as v${head}`,
    'D0005: release.yml publishes only when the tag is new, so merging this ships NOTHING even though the version is ahead of main. Bump past the tag.')]
}

// ── New dependencies vs. the build budget (D0004) ──────────────────────────
// Names that mean a bundled-C/C++ or native-linking tree, or a tree that alone would eat the
// 60s clean-build budget. `[-_]sys$` catches the name, not the tree, so the pure-Rust `-sys`
// crates are named as exceptions.
const FORBIDDEN_DEP = /^(?:.*[-_]sys|libduckdb(?:[-_].*)?|duckdb|rocksdb|librocksdb(?:[-_].*)?|openssl|native-tls|aws-lc-rs|aws-lc-.*|ring|zstd|lz4|bzip2|snappy|cc|cmake|bindgen|arrow(?:[-_].*)?|parquet|datafusion(?:[-_].*)?|polars(?:[-_].*)?|tikv-jemallocator|jemallocator|mimalloc)$/i
const SYS_NAME_ONLY = /^(js-sys|web-sys|windows-sys|linux-raw-sys)$/
const DEP_TABLE = /^\[(?:target\.[^\]]+\.)?(dependencies|dev-dependencies|build-dependencies)(?:\.([A-Za-z0-9_-]+))?\]/

// Names from the dependency tables only. Reading the two Cargo.toml versions beats
// scanning the diff: `+version = "0.1.0"` in [package] is not a new dependency.
export function depNames(cargo) {
  const names = new Set()
  let inTable = false
  for (const line of (cargo || '').split('\n')) {
    const header = line.match(/^\[/) ? line.match(DEP_TABLE) : null
    if (line.startsWith('[')) {
      // Inside `[dependencies.foo]` the following lines are foo's fields, not deps.
      inTable = !!header && !header[2]
      if (header && header[2]) names.add(header[2])
      continue
    }
    if (!inTable) continue
    const m = line.match(/^\s*([A-Za-z0-9_-]+)\s*=/)
    if (m) names.add(m[1])
  }
  return names
}

export function newDeps(baseCargo, headCargo) {
  const out = []
  const before = depNames(baseCargo)
  const added = [...depNames(headCargo)].filter(n => !before.has(n))
  for (const name of added) {
    if (FORBIDDEN_DEP.test(name) && !SYS_NAME_ONLY.test(name)) {
      out.push(finding('forbidden-dep', 'error', 'Cargo.toml', 1,
        `new dependency \`${name}\` looks like a bundled-C / native-linking / heavy tree`,
        'D0004: a bundled-C or native-linking crate, or one that blows the 60s clean build, is a design change. File an issue first.'))
    } else {
      out.push(finding('new-dep', 'warn', 'Cargo.toml', 1,
        `new dependency \`${name}\` — confirm the clean build stays under the 60s budget`,
        'D0004: judge it by build-and-ship cost (compile time, toolchain, binary size), not by whether it is pure Rust. CI\'s build-budget job times it clean.'))
    }
  }
  return out
}

// ── Test placement: one integration test binary, tests/e2e/ ────────────────
// Every tests/*.rs, and every tests/<dir>/main.rs, is its own crate and link step. adelie
// keeps exactly one: tests/e2e/main.rs, with each suite a module under tests/e2e/.

export function testPlacement(addedFiles) {
  const out = []
  for (const f of addedFiles) {
    const topLevel = /^tests\/[^/]+\.rs$/.test(f)
    const otherBinary = /^tests\/(?!e2e\/)[^/]+\/main\.rs$/.test(f)
    if (!topLevel && !otherBinary) continue
    out.push(finding('test-placement', 'error', f, 1,
      topLevel ? 'new top-level tests/*.rs file' : 'new integration test binary outside tests/e2e/',
      'Each is a separate test binary with its own link step. Add a module under tests/e2e/ (declared from tests/e2e/main.rs) instead; pure-logic tests go inline in their module.'))
  }
  return out
}

// ── Miri ignores must name their reason ─────────────────────────────────────
// ci.yml: a test Miri cannot run carries `#[cfg_attr(miri, ignore)]` with its reason. The
// reason may trail the attribute or sit on the comment line directly above it.

const MIRI_IGNORE = /#\[cfg_attr\(miri,\s*ignore\)\]/
const DOCUMENTED_IGNORE = /#\[cfg_attr\(miri,\s*ignore\)\]\s*\/\/\s*\S/
const REASON_ABOVE = /^\s*\/\/\/?\s*\S/

export function miriIgnore(text, addedLines, file) {
  const lines = text.split('\n')
  const out = []
  for (let i = 0; i < lines.length; i++) {
    if (!MIRI_IGNORE.test(lines[i])) continue
    if (addedLines && !addedLines.has(i + 1)) continue
    if (DOCUMENTED_IGNORE.test(lines[i])) continue
    if (i > 0 && REASON_ABOVE.test(lines[i - 1])) continue
    out.push(finding('miri-ignore', 'error', file, i + 1,
      'Miri ignore with no stated reason',
      'ci.yml (Miri job): a test Miri cannot run (fsync, unsupported syscalls) says why, as `#[cfg_attr(miri, ignore)] // <reason>` or a comment on the line above. Pure-logic tests must run under Miri; if there is no reason, drop the ignore.'))
  }
  return out
}

// ── Session links stay out of history ──────────────────────────────────────
// AGENTS.md: never put session links in PR bodies or commit messages. Both are permanent
// and public; a session link is neither.

const SESSION_LINK = /claude\.ai\/code\/session/i

export function sessionLink(texts = []) {
  const out = []
  for (const { source, text } of texts) {
    const lines = String(text || '').split('\n')
    const i = lines.findIndex(l => SESSION_LINK.test(l))
    if (i === -1) continue
    out.push(finding('session-link', 'error', source, i + 1,
      `${source} contains a claude.ai/code/session link`,
      'AGENTS.md: never put session links in PR bodies or commit messages. Reword the commit (git commit --amend / rebase) or edit the PR body to drop it.'))
  }
  return out
}

// ── Tickets this change ships but does not close ───────────────────────────
// A bare mention closes nothing, so the bead silently outlives the work that finished it.
// `acknowledged` (Refs/Part of/See) states the disposition without claiming to close it.
export function unclosedTickets(mentioned = new Set(), closing = new Set(), titles = {}, acknowledged = new Set()) {
  return Array.from(mentioned)
    .filter(ref => !closing.has(ref) && !acknowledged.has(ref) && titles[ref])
    .map(ref => finding('stale-ticket', 'warn', 'PR body', 1,
      `${ref} is worked by this change but nothing states it will be closed`,
      `"${titles[ref]}". AGENTS.md: close the ticket yourself when the PR merges. Add "Closes ${ref}" to the PR body, or "Refs ${ref}" if this change does not finish it. Nothing auto-closes — on merge, run bd close ${ref} and bd dolt push.`))
}

// ── The /adelie skill's lane files must stay wired ──────────────────────────
// SKILL.md is a router: it names a lane file per subcommand and the lane body lives there,
// so a rename that misses one silently drops a whole subcommand (found in nidus, nidus-gmy.2).

export function skillLanes(skillText, laneFiles) {
  const referenced = new Set([...skillText.matchAll(/lanes\/([a-z-]+)\.md/g)].map(m => m[1]))
  const present = new Set(laneFiles.map(f => f.replace(/^.*\//, '').replace(/\.md$/, '')))
  const out = []
  for (const name of [...referenced].sort()) {
    if (present.has(name)) continue
    out.push(finding('skill-lane-missing', 'error', '.claude/skills/adelie/SKILL.md', 1,
      `SKILL.md routes to lanes/${name}.md, which does not exist`,
      'That subcommand has no body to read, so the lane silently does nothing.'))
  }
  for (const name of [...present].sort()) {
    if (referenced.has(name)) continue
    out.push(finding('skill-lane-orphan', 'warn', `.claude/skills/adelie/lanes/${name}.md`, 1,
      `lanes/${name}.md is not routed to from SKILL.md`,
      'Nothing will ever read it. Add it to the Routing table or delete it.'))
  }
  return out
}

// ── The context budget and its pointers ────────────────────────────────────
// AGENTS.md is the one instruction file and loads into every session and every subagent,
// so its size is paid per agent.

export const AGENTS_MD_MAX = 200

export function contextBudget(agentsMd) {
  if (agentsMd == null) return []
  const n = agentsMd.replace(/\n$/, '').split('\n').length
  if (n <= AGENTS_MD_MAX) return []
  return [finding('context-budget', 'error', 'AGENTS.md', 1,
    `AGENTS.md is ${n} lines — the cap is ${AGENTS_MD_MAX}`,
    'It loads into every session and every subagent. Move rationale to decisions/ and leave a D#### pointer.')]
}

// A `D####` pointer is the whole mechanism for keeping rationale out of context, so a
// dangling one silently loses the reason a rule exists.

export function decisionPointers(texts, decisionFiles) {
  const present = new Set(decisionFiles.map(f => (f.match(/^(\d{4})/) || [])[1]).filter(Boolean))
  const out = []
  for (const { file, text } of texts) {
    const seen = new Set()
    for (const m of (text || '').matchAll(/\bD(\d{4})\b/g)) {
      if (present.has(m[1]) || seen.has(m[1])) continue
      seen.add(m[1])
      out.push(finding('dangling-decision', 'error', file, 1,
        `cites D${m[1]}, which is not in decisions/`,
        'The rule keeps its one-liner and the reasoning lives in the record. A dangling pointer loses the reasoning.'))
    }
  }
  return out
}

export const LAW_IDS = [
  'unsafe-code', 'unsafe-attr', 'stale-base', 'empty-scope',
  'version-bump', 'version-backwards', 'version-already-tagged', 'forbidden-dep', 'new-dep',
  'test-placement', 'miri-ignore', 'session-link', 'stale-ticket',
  'skill-lane-missing', 'skill-lane-orphan', 'context-budget', 'dangling-decision',
]
