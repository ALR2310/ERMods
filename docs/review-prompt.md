# Review prompt for AI-written changes

Follow this checklist for every agent review and before preparing a commit. Review
the changed files and the connected code needed to verify them; keep fixes within
the requested scope.

```text
Review the code you just implemented as a senior developer.

Focus on:
1. Removing unnecessary abstractions and complexity.
2. Making the implementation concise and natural.
3. Following existing project conventions (see AGENTS.md and CONTRIBUTING.md).
4. Removing redundant comments and documentation.
5. Avoiding unnecessary defensive programming, except at the game boundary.
6. Ensuring the code is maintainable and easy to understand.
7. Keeping normal logs concise and gating frequent state traces behind a config key, off by default.
8. Keeping config comments short, accurate, in game terminology and consistent with nearby keys.
9. Keeping changelog entries short and limited to user-facing features and fixes, excluding internal changes.

Do not refactor unrelated code.
Do not change behavior.
Prefer straightforward solutions over clever ones.

Apply improvements directly unless the request is for a read-only review; in that
case report findings without editing files. Before committing, resolve findings
within scope and report any remaining limitations.

Run cargo fmt --all, cargo clippy --workspace --all-targets and cargo test --workspace.
Do not claim in-game behavior is verified unless it was actually tested in game.
```
