export const meta = {
  name: 'adelie-spec',
  description: 'Research an adelie change from four fixed lenses and propose the directory partition the blueprints will follow',
  phases: [
    { title: 'Research' },
    { title: 'Partition' },
  ],
}

// args (from the /adelie spec path):
// { id: "adelie-vnn" | null, ask: "<ticket text or description>" }
//
// Returns research only. The MAIN thread writes the blueprints from it — the gate the
// user approves should be Opus-authored, not a sonnet summary.
const cfg = typeof args === 'string' ? JSON.parse(args) : (args || {})

const RESEARCH_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['lens', 'files', 'findings', 'risks'],
  properties: {
    lens: { type: 'string' },
    files: {
      type: 'array',
      items: {
        type: 'object',
        additionalProperties: false,
        required: ['path', 'role'],
        properties: {
          path: { type: 'string' },
          role: { type: 'string', description: 'What this file does and why it matters here' },
          symbols: { type: 'array', items: { type: 'string' } },
        },
      },
    },
    findings: { type: 'array', items: { type: 'string' } },
    patterns: {
      type: 'array',
      description: 'Concrete code to mirror: path, line range, and what the pattern is',
      items: {
        type: 'object',
        additionalProperties: false,
        required: ['path', 'lines', 'what'],
        properties: { path: { type: 'string' }, lines: { type: 'string' }, what: { type: 'string' } },
      },
    },
    risks: { type: 'array', items: { type: 'string' } },
  },
}

const PARTITION_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['summary', 'groups', 'ordering', 'scope_questions', 'open_questions'],
  properties: {
    summary: { type: 'string' },
    groups: {
      type: 'array',
      description: 'Ordered groups; everything in one group can be implemented in parallel',
      items: {
        type: 'array',
        items: {
          type: 'object',
          additionalProperties: false,
          required: ['dir', 'scope', 'verify'],
          properties: {
            dir: { type: 'string' },
            scope: { type: 'string', description: 'What changes here' },
            verify: { type: 'array', items: { type: 'string' } },
          },
        },
      },
    },
    ordering: { type: 'array', items: { type: 'string' }, description: 'Why the groups are ordered that way' },
    scope_questions: {
      type: 'array',
      description: 'Forks that change WHICH files exist, so they must be settled before any blueprint is written',
      items: {
        type: 'object',
        additionalProperties: false,
        required: ['question', 'forks', 'options'],
        properties: {
          question: { type: 'string', description: 'Asked of the developer, answerable without reading the code' },
          forks: { type: 'string', description: 'What changes in the unit list and file list depending on the answer' },
          options: {
            type: 'array',
            items: {
              type: 'object',
              additionalProperties: false,
              required: ['label', 'implication'],
              properties: {
                label: { type: 'string', description: 'Four words or fewer' },
                implication: { type: 'string', description: 'What this answer adds to or drops from the change' },
              },
            },
          },
          recommendation: { type: 'string', description: 'Which option this partition would pick, and one line of why' },
        },
      },
    },
    open_questions: {
      type: 'array',
      description: 'Small reversible details that belong in a blueprint, NOT questions for the developer',
      items: { type: 'string' },
    },
  },
}

const CONTEXT = `adelie is a pure-Rust columnar analytics store with SQL: immutable column segments on local
disk under an atomically published manifest, no WAL (a write is acknowledged once its segment
is durable), a vectorized morsel-parallel engine, a hand-rolled SQL front end and rule-based
planner, OpenTelemetry ingest, and built-in MCP. Read AGENTS.md first, then SPEC.md: \`.claude/skills/adelie/bin/spec toc\` is the index, \`spec find <words>\`
says which section covers a topic, and \`spec <ref>\` prints just that one. §2 lists the non-goals
and §15 the open questions, so fetch them before proposing something new.`

const LENSES = [
  {
    key: 'modules',
    prompt: `Map the code this change must touch. Which modules own the behaviour today, what are the
seams, and where would a new concern go? A module that spans several concerns is a directory
of sibling files (SPEC §14 is the target layout), so say which FILE each piece belongs in, not just which
directory. Note anything already feature-gated.`,
  },
  {
    key: 'tests',
    prompt: `Map how this area is tested and verified. Inline pure-logic tests, file-backed tests in
tests/, e2e modules under tests/e2e/ that drive the real binary, sqllogictest files, crash tests,
Miri discipline (what must run under it, what may be #[cfg_attr(miri, ignore)]d),
and which \`just\` recipe is the real gate. Give concrete examples to copy.`,
  },
  {
    key: 'laws',
    prompt: `Find the repo laws this change will collide with: the core commitments (SPEC §1), feature
gating and the lean build, the build budget (D0004), \`#![deny(unsafe_code)]\`, additive-only
on-disk formats (SPEC §5), the durability order (SPEC §6), and "a feature ships whole" (SPEC §4:
core, HTTP, CLI, MCP and docs in one PR). Quote the AGENTS.md lines and decisions that apply.`,
  },
  {
    key: 'prior-art',
    prompt: `Find prior art. Search the tracker for related issues, open and closed
(\`bd search "<terms>"\`, \`bd list --all\`), fetch the relevant spec section with
\`bin/spec find <terms>\` then \`bin/spec <ref>\` (§2 non-goals, §15 open questions), read
\`decisions/\`, and read git history
(\`git log --oneline\`, then \`git show\`) for earlier attempts or decisions about this area.
Report what was already decided and why, so this change does not relitigate it.`,
  },
]

phase('Research')

const research = await parallel(LENSES.map(l => () => agent(
  `${CONTEXT}

THE ASK${cfg.id ? ` (${cfg.id})` : ''}:
${cfg.ask}

YOUR LENS — ${l.key}:
${l.prompt}

Read real files and quote real paths and line numbers. Do not propose an implementation and do
not edit anything; this is research that another agent will build a plan from.`,
  { label: `research:${l.key}`, phase: 'Research', model: 'sonnet', schema: RESEARCH_SCHEMA },
)))

const bundle = research.filter(Boolean)

phase('Partition')

const partition = await agent(
  `${CONTEXT}

THE ASK${cfg.id ? ` (${cfg.id})` : ''}:
${cfg.ask}

Four researchers reported:
${JSON.stringify(bundle, null, 2)}

Propose how to split the implementation into directory-scoped units that can be built in
parallel without touching each other's files. Rules:
- One unit owns one directory (or one file set); units in the same group MUST be file-disjoint.
- Put a unit in a later group only when it genuinely depends on an earlier one's code.
- Each unit's \`verify\` is the exact just recipes that cover it. Remember \`just ci\` does NOT
  compile src/cli, src/server or src/bin — those need \`just ci-cli\`; the MCP surface needs
  \`--features mcp\`. Never list \`just miri\`: CI's required Miri job covers codec/kernel
  changes on the PR, and the interpreter is too slow for a unit's verify loop.
- Flag anything that must stay in ONE unit because splitting it would break the build.

Then split what you do not know into two piles, because they are consumed differently.
\`scope_questions\` are forks the developer must settle BEFORE any blueprint is written: each one
changes which units exist or which files they touch, and answering it wrong is expensive to walk
back. Which surfaces are in scope (core / HTTP / CLI / MCP / the three SDKs / docs), whether an
on-disk or wire format changes, whether an existing default moves, whether this supersedes or
sits beside something that already ships. Ask only what the ticket and the research do not
already answer, phrase each so it can be answered without reading the code, and give concrete
options with their consequences. If the ask is genuinely unambiguous, return an empty array
rather than inventing a question. Everything else — a name, a constant, an ordering that is
cheap to revise — is an \`open_question\`, and goes in a blueprint rather than to the developer.`,
  { label: 'partition', phase: 'Partition', schema: PARTITION_SCHEMA },
)

return { id: cfg.id, research: bundle, partition }
