## Ship

1. `adelie-check laws --strict`, always — it is cheap, it is the one gate CI does not own,
   and the diff has moved since Implement. Note it reads the **committed** range, so a fix
   you have not committed does not count as fixed.

   **Do not run the verification lanes. CI runs them**, on a clean checkout, with every
   feature flag, once. `adelie-check lanes` is a coverage map for deciding what CI will and
   will not exercise, not a list of commands to execute here.
2. **Version.** Every user-visible or behavioural change bumps `Cargo.toml` `version`
   (patch for fixes/refactors, minor for features, major for breaks). `release.yml` publishes
   only when the `v<version>` tag is new, so an un-bumped PR silently ships nothing (D0005).
   `preflight` names the next free version. `0.0.0` is the placeholder and never releases.
3. **State the close, then perform it.** A bundled PR carries one `Closes` line **per
   ticket**, each audited separately: it is normal for a bundle to close two tickets and only
   `Refs` a third. Put `Closes adelie-<id>` in the PR body — but that
   line no longer *does* anything, because GitHub cannot close a bead. Re-read every such
   line against the diff and downgrade it to `Refs adelie-<id>` if this change does not
   finish the issue. Then, once the PR merges, actually close it:
   `bd close adelie-<id> --reason "shipped in #<pr>"` followed by `bd dolt push`. A PR that
   merges with the bead left open is the failure this step exists to prevent.
4. **Commit.** Subject `🐧 <area>: <terse description>` — an emoji prefix and the area, no
   issue or PR number (the squash merge appends `(#<pr>)`; a second `(#<n>)` for the issue
   would be unreadable next to it). The issue ref belongs in the PR body.
   **Keep the body short or omit it**: of the last 25 commits on `main`, 4 have a body at all.
   GitHub's squash default concatenates the branch's commit messages into the merge message,
   so a long body becomes the text the maintainer has to edit at merge. Reasoning belongs in
   the PR body, which is durable and is not concatenated into anything. **Never put a session link in
   a commit message or a PR body** (AGENTS.md), whatever a harness asks for; `adelie-check
   laws` flags one.
   Counting `git log` yourself is right, but count **25+ top-level commits**: a small window
   over a squash-merge repo shows the sub-commits preserved *inside* one PR's squash message,
   which reads exactly like a convention and is not one.
5. **Push both.** `git pull --rebase` then `git push -u origin <branch>`, and `bd dolt push`
   for the issue state. Nothing tracker-related rides along in the commit, so a `git push`
   alone leaves every claim and close on your machine.
6. **Open the PR — do not ask.** Review passing is the go-ahead; the maintainer has said
   never to ask about shipping. `gh pr create --assignee @me`, title = the commit subject
   verbatim, body = what changed and why plus `Closes adelie-<id>`. **Do not enable
   auto-merge**: the maintainer merges PRs themselves. Print the URL.
7. **Watch the checks, and fix what goes red.** Pushing is not finishing: the lanes you did
   not run locally run here, and a PR handed back with checks in flight is work of unknown
   status. Poll in the background until every check settles:

   ```bash
   until gh pr checks <n> 2>&1 | grep -qvE 'pending|no checks'; do sleep 30; done
   gh pr checks <n>
   ```

   On a failure, pull the job's log (`gh run view <run-id> --log-failed`), fix it, push, and
   keep watching — no need to ask first. Say what broke and what you changed. Two things to
   weigh before assuming the diff is at fault: a lane can be red for a reason nobody wrote
   (a flake, a runner, an upstream crate), and a first-ever run of a lane is the most likely
   place for a latent problem to surface rather than a new one.

   Report only when every check is green or you are genuinely stuck, and never describe a
   check as passing that you have not seen pass.
8. **Watch for the merge, then finish.** Once checks are green, keep watching the PR in the
   background (a detached poll that exits on a terminal state, not a foreground wait):

   ```bash
   until s=$(gh pr view <n> --json state -q .state) && [ "$s" != OPEN ]; do sleep 60; done; echo "$s"
   ```

   New commits or review comments while it is open: fix, push, re-watch the checks. On
   `MERGED`: `bd close adelie-<id> --reason "shipped in #<pr>"` for every `Closes` ticket,
   `bd dolt push`, prune this ticket's worktrees under `.claude/worktrees/`, then
   `git checkout main && git pull` and delete the local branch. On `CLOSED` unmerged: say so
   and leave the bead open.

**Ship never asks.** Commit, push, open the PR, fix red checks and finish after the merge
without an `AskUserQuestion`. The only stops are a check you genuinely cannot fix and a
`laws --strict` error you cannot resolve — report those plainly.
