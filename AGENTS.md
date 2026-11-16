# Celld fork maintenance

Read [FORK.md](FORK.md) before changing this fork. Upstream architecture and
runtime contracts remain in [README.md](README.md) and `docs/`.

Base each refresh on freshly fetched upstream main, verify the selected release,
and carry only necessary fork patches as clean linear commits. Keep each patch's
intent reviewable; remove superseded changes and fix up local branch commits.
Do not add merge commits, rewrite shared main, force-push shared history, delete
branches, or alter historical release tags. Preserve historical recovery refs.

All Harrhy applications use one qualified Celld artifact, not per-app forks or
versions. Record immutable source, image and executable identities; validate the
actual embedded Worker runtime as well as storage and CLI behavior. Isolated
candidate tests are allowed before qualification. Coordinate a common fleet pin
with the Harrhy source owner and runtime replacement with the sole operator.
Bounded rolling replacement may overlap versions temporarily; persistent mixed
versions are unsupported. Never silently update a running preview binary,
reseed private state, change an issuer, or treat release publication as cutover.

Use the existing fork release workflow and `fork.json`; do not invent a separate
release platform. Keep the upstream security dependency floors. Run affected
workspace tests, clippy and the standalone storage harness; crypto changes also
need standards vectors and actual Worker integration, not Node-only mocks.
Document missing checks honestly before proposing an artifact for fleet use.
