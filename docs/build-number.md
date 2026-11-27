# Port build number

`BUILD_NUMBER` at the repository root is the canonical integer build counter.
It starts at 125 by convention, not a count of historical commits. Display
version is `0.<counter>` and is independent of the Cargo package version and
the Xbox title build ID.

For every update published to main, increment the counter by exactly one in
the same commit as the update. A push containing multiple commits is still
one published update. The CI checks the before/after values on pushes to main
and compares a pull request against its base. CI reports an invalid increment;
it does not push an extra version commit. Contributors should update the
counter before pushing (or before merging a pull request).

Cargo embeds the counter at compilation and rebuilds the affected crates when
it changes. Rebuilding the same revision does not change the counter. Both
release and diagnostics builds use it. It appears in the window caption, the
startup log (port build), the CSV's final numeric build_number column, and
`victory --version`. Existing executables must be recompiled to show the
new number.
