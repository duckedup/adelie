## Fit

Feature thought work, before any spec: is this the right fit, does it make sense, would
users use it? Read-only — no branch, no blueprints, no code. The target is an idea (a
description, or an issue number to resolve with `bd show`).

1. **Check it is not already decided.** Search open and closed issues
   (`bd search "<terms>"`, `bd list --all --label decision`), SPEC §2's non-goals, SPEC §15's open questions, and `decisions/` for prior art, so a
   rejected idea is still findable. One returning without new
   evidence gets the old answer, cited.
2. **Gap or feature?** If the product already implies the capability (a doc describes it,
   a surface half-has it, sibling surfaces have it and this one lacks it), it is a **gap**:
   skip fit, file it as a `task`/`bug` with the evidence, done. Fit is for genuinely new
   capability.
3. **Right fit.** Judge against SPEC §1 (the core foundation and thesis) and §2
   (goals/non-goals): does it belong in adelie core, or in the host application, the docs,
   or a separate tool? Does the public positioning (SPEC §1 and §4: the shipped surfaces,
   nothing promised beyond what ships) survive it?
4. **Does it make sense — the cost side.**
   - On-disk or wire format change? SPEC §5's rule applies: formats are additive only, and a format
     change needs a **named caller**; query-path features are judged on their own merits.
   - New dependency? The build budget is a CI-enforced gate; a heavy dep is a design
     change, not an implementation detail.
   - New surface? Every surface owes a load-bearing test in CI (§11), and a new server
     capability ships whole (SPEC §4): core, HTTP, CLI, MCP and docs — count
     that cost, not just the core diff.
5. **Would users use it — name the caller.** A concrete user or workflow that is blocked
   or degraded today, what they do instead (the workaround is evidence), and what changes
   for them if this ships. "It would be nice" names nobody.
6. **Verdict**, recorded durably, one of:
   - **Pursue**: file the issue (`feature` label, priority argued from the caller), with
     the assessment as the body. Offer to continue into **Spec**.
   - **Defer with a trigger**: file a `decision`-labeled issue naming the condition that
     reopens it ("revisit when a caller asks for X"), recorded as a SPEC §15 open question or a decision.
   - **Reject**: file or comment the decision with the reason, so the next person who has
     the idea finds the answer instead of re-deriving it. Format-adjacent rejections also
     earn a decision record in `decisions/`.

The gate the user sees is the verdict and its reasoning, not a wall of research. Three
sharp paragraphs beat ten pages.

