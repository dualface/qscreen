# Spec Adversarial Review

workflow id: `wf-multi-attach-clients-20260522-36f5`

reviewed spec path: `/Users/dualface/Desktop/Works/qscreen/worktrees/multi-attach-clients/docs/specs/wf-multi-attach-clients-20260522-36f5.md`

verdict: `pass`

## critical_findings

none

## advisory_findings

- The spec says non-attach command dispatch can keep existing `Input` and `Resize` support, while normal attached-client semantics live in `handle_attach`. This is executable, but implementation should make clear whether legacy non-attach `Input`/`Resize` paths are test-only/control paths or still user-reachable daemon commands, so they do not accidentally bypass client-id based sizing semantics.
- The spec requires detaching on failed initial response, failed scrollback write, writer-task shutdown, explicit detach, and client disconnect. This is good coverage, but implementation should centralize cleanup so these paths cannot double-remove or leak a client if two shutdown signals race.

## external_dependency_risks

- dependency: Terminal focus reporting
  risk: Some terminals, SSH paths, nested terminal environments, or Windows terminal combinations may not emit focus-gained events consistently.
  suggested mitigation: Keep input-triggered resize mandatory, ignore focus-lost, and include manual validation on the supported terminal combinations.
- dependency: `crossterm` focus event support
  risk: Implementation depends on `crossterm = 0.28` exposing focus-gained events consistently for target platforms.
  suggested mitigation: Verify the pinned API during implementation; if a platform lacks support, use compile guards or raw focus-sequence parsing only where needed.

## assumptions made

- `docs/templates/spec-review-template.md` and `docs/templates/worker-result-template.json` were not present in this worktree, so this review uses the fields required by the prompt directly.
- No previous review was provided, and the spec says review history is `none`.
- No repo code needed to be read because the spec was complete enough to determine verdict.

## recommended next action

Proceed to user spec approval, while calling out the two external dependency risks before approval.
