// PreToolUse gate: an unbounded read of a large doc `bin/spec` can address pays for the
// whole file to use one section. Quiet until SPEC.md passes GUARD_MIN_LINES (800).

import { readFileSync, existsSync } from 'node:fs'
import { resolve, relative } from 'node:path'

import { specRead, GUARDED } from '../skills/adelie/lib/guards.mjs'

const repo = resolve(new URL('../..', import.meta.url).pathname)

let input
try { input = JSON.parse(readFileSync(0, 'utf8')) } catch { process.exit(0) }

const args = input?.tool_input ?? {}
const countLines = p => {
  try { return existsSync(p) ? readFileSync(p, 'utf8').split('\n').length : 0 } catch { return 0 }
}
// Only a guarded doc is read to count its lines; every other Read costs nothing extra.
const rel = args.file_path ? relative(repo, resolve(args.file_path)) : null
const message = rel && GUARDED[rel]
  ? specRead({ rel, offset: args.offset, limit: args.limit, lines: countLines(resolve(args.file_path)) })
  : null

if (!message) process.exit(0)
console.error(message)
process.exit(2)
