# nogent prompts

The **system prompts** for the AI scan live here as Markdown, out of the Rust
source, so they can be edited without recompiling and reused across projects.

| File | Used for |
|------|----------|
| `pr-review.system.md` | PR security review — role, the review guidelines (what to look for), what CI already covers (don't duplicate), prompt-injection defense, and the strict JSON output contract. |
| `issue-triage.system.md` | Issue triage — role, non-hallucination rules, injection defense, output contract. |

## Placeholders

- `{{canary}}` — replaced at runtime with a per-run random token the model must
  echo back. The [output validator](../src/output_validator.rs) rejects any
  response that omits or alters it (prompt-injection backstop). **Keep the
  `{{canary}}` references** when editing.

The *user* prompt (the event facts + the bounded, `<untrusted_*>`-tagged diff /
issue body) is assembled in code (`../src/prompts/`), not here — it carries data
and a security mechanism, not guidance.

## How they're loaded

At runtime, for each file:

1. if `NOGENT_PROMPTS_DIR` is set and `$NOGENT_PROMPTS_DIR/<file>` reads, use it;
2. otherwise use the copy embedded in the binary at build time (these files).

So the binary always works out of the box, and you can override without a
rebuild.

## Reusing in another project

1. Copy this `prompts/` directory somewhere (or vendor it into the other repo).
2. Edit the **role line** and the **`## Security model` / guidelines** sections
   for that codebase — that's the project-specific part. Leave the injection
   defense, non-hallucination, and output-contract sections as-is, and keep
   `{{canary}}`.
3. Run nogent with `NOGENT_PROMPTS_DIR=/path/to/your/prompts`.

The JSON schema in the output contract must stay in sync with the structs in
`../src/output_validator.rs` — if you add/rename a field there, update it in the
Markdown too (and vice versa).
