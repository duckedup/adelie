// Pure decisions for the PreToolUse guards. No IO, so selftest.mjs drives them from
// fixtures and a guard that stops firing fails there rather than silently.

/// Docs big enough that a whole-file read is never what the reader wanted, and the tool
/// that addresses them by section instead.
export const GUARDED = { 'SPEC.md': 'just spec' }

/// nidus blocked every unbounded SPEC.md read, at any size (its spec was ~177KB). adelie's
/// is ~400 lines, which one read handles fine, so the guard stays quiet until it passes 800.
export const GUARD_MIN_LINES = 800

function advice(rel, how, lines) {
  return `Do not read ${rel} whole (${lines} lines) — use \`${how}\` instead:

  ${how} toc            the section index, with line counts
  ${how} find <words>   which section covers a topic
  ${how} <ref>          print one section (7, 7.4, 7.4.1, or a slug)

(\`${how}\` is .claude/skills/adelie/bin/spec if the recipe is missing.) A whole-file read
spends its tokens to use one section, and every subagent pays it again. If you need a
specific line range, pass Read both offset and limit.`
}

/// A bounded read (both offset and limit) is allowed: that caller already knows the range.
/// `lines` is the doc's current length; unknown or under the threshold is allowed.
export function specRead({ rel, offset, limit, lines } = {}) {
  if (!rel) return null
  if (offset !== undefined && limit !== undefined) return null
  const how = GUARDED[rel]
  if (!how) return null
  if (!(Number(lines) > GUARD_MIN_LINES)) return null
  return advice(rel, how, lines)
}

// A Bash matcher on command text was tried in nidus and removed: it fired on its own test
// fixture, because a command that merely *mentions* a read is not one. Structured tool
// input is the only reliable layer, so the guard covers Read alone.
