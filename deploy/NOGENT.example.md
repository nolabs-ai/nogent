# Reviewing this repository

Drop this file at your repo root as `NOGENT.md` (or `.github/nogent.md`) to give
nogent project-specific review guidance. It's freeform Markdown — write whatever
helps a reviewer understand your conventions.

nogent reads it from the **base branch** of a PR (not the PR head), so it's
trusted maintainer guidance: a fork PR cannot change how it gets reviewed. It is
appended to the reviewer's instructions but cannot override the output contract
or the prompt-injection rules.

## Examples of useful guidance

- **Conventions:** "All errors use `MyError`; flag `anyhow` in library crates."
- **Focus:** "Pay special attention to the `auth/` and `crypto/` modules."
- **Scope:** "Don't comment on formatting or import ordering — pre-commit handles it."
- **Context:** "`src/legacy/` is frozen; only flag security issues there."
- **Severity:** "Treat any new `unsafe` without a `// SAFETY:` comment as blocking."
- **Domain:** "This is a parser; watch for unbounded recursion and allocation on
  untrusted input."

Keep it concise (it's capped at ~16 KB). Think of it as the note you'd give a new
human reviewer on their first day.
