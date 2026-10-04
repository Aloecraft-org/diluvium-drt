# Queue

Work the operator assigns to the owner. States: open, answered.
To answer: write under Answer and change the state to `answered`.
The operator applies answered items and deletes them.

## Q-001 Approve and publish v0.8.0-rc.1 [open]
Asked: 2026-10-03
Why: v0.7.0, the stable release, has no browser-access host. Downstream
builds pin dev builds, and dev builds are pruned once ten newer ones
exist; v0.8.0-dev.19 is the oldest still kept and goes with the next
one. A candidate is what keeps a pin working.
Prepared: https://github.com/Aloecraft-org/diluvium-drt/pull/46 (the cut
entry, `.technoproj` and the version stamps; the code is v0.8.0-dev.27's,
whose browser and browser-access-client jobs passed). Cutting a release
is not in authority.yaml's autonomous tier, so it waits for you.
Question: cut v0.8.0-rc.1 from #46? If yes: merge #46, run the Release
rehearsal on main with tag=v0.8.0-rc.1 and publish off, then push the
tag from your account. The steps are in #46's description.
Answer:
