# ADR-004: Opt-out is honored, not concealed

Accepted, 2026-07-31.

## Context

A maintainer can ask us to stop tracking their repo, and we stop: the badge reads not tracked, the repo leaves the rankings, and nothing further is collected.
The stored readings stay, because the store is append-only and nothing is ever deleted from it, but nothing is shown.

The wording we use publicly is that an opted-out repo "reads as never tracked".
That is a claim about what we print, and it is true.
It is easy to read it as a stronger claim, that an outside observer cannot tell the two apart at all, and that stronger claim is one we cannot honestly make.
An external security review asked exactly this, so it is worth settling once rather than each time a surface is added.

The mechanics: a badge request for a repo we have never seen enrolls it, and the badge then says tracking started today.
A request for an opted-out repo returns the not-tracked badge and enrolls nothing.
So for a repo that demonstrably exists on GitHub, a not-tracked badge already narrows the possibilities, and anyone can check GitHub.
Separately, the two paths take different amounts of time: answering for an opted-out repo is one indexed lookup, while a name we have never seen costs a live GitHub call, which is a couple of hundred milliseconds of difference.

## Decision

The promise is about output, and it is absolute there: every surface renders an opted-out repo exactly as it renders one we never saw, byte for byte, with no distinguishing reason, code, or hint.

The promise does not extend to unobservability, and we do not pretend otherwise.

We will not close the timing difference. Padding the fast path with an artificial delay buys nothing real, and the alternative, making the same GitHub call for an opted-out repo so the paths cost the same, would mean continuing to make requests about a repo whose owner asked us to stop. That is worse than the thing it fixes.

What the observable difference can actually reveal is bounded and small: it separates "opted out" from "no such repo". Anyone who wants that distinction can ask GitHub directly, so nothing here leaks a fact the observer could not already get.

Binding rules for every future surface, which is the point of writing this down:

- No surface ever states or implies a reason. Not tracked never becomes "removed at owner request", not in copy, not in an aria-label, not in an error body, not in an API field.
- No surface exposes opted-out repos in aggregate. No count, no list, no "n repos have opted out", no filterable status.
- Error and empty states for opted-out and never-tracked stay literally the same string. Anything that would need to branch on the difference is a design mistake, not an implementation detail.
- The suppression check is terminal, never a hint to re-enroll. Both the name lookup and the post-lookup check by numeric repo id must treat it that way, and both must fail closed if the check itself errors.

## Consequences

- A maintainer gets what they actually asked for, which is to stop being tracked and displayed, and gets it without us having to lie to anyone about why.
- The identity check is by numeric repo id and not by name, so an opted-out repo that is later renamed stays suppressed instead of quietly re-enrolling under its new name. That is the case worth guarding, because it is invisible until someone hits it.
- Adding an authenticated API later does not get to relax any of this. A caller with a key sees the same not-tracked answer as the public badge.
- We accept, and state here, that someone comparing our answer against GitHub can infer a repo opted out. Writing that down is cheaper than defending a promise we never meant to make.
