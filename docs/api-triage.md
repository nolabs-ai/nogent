# `POST /api/triage` — grounded Q&A triage

Answer a question grounded on **caller-supplied context**, returning a
canary-validated decision. Stateless and source-agnostic: no GitHub, no tarball,
no repo index, nothing posted anywhere — the caller owns retrieval and any
downstream posting.

A machine-readable contract lives in [`openapi.yaml`](./openapi.yaml).

## Enabling the endpoint

The route is always mounted but **fail-secure**: it returns `404` unless
`NOGENT_TRIAGE_SECRET` is set. It works with or without the GitHub integration
(see question-only mode in [`../DEPLOY.md`](../DEPLOY.md)).

## Auth

```
Authorization: Bearer <NOGENT_TRIAGE_SECRET>
Content-Type: application/json
```

The bearer token is compared in constant time. Missing/invalid → `401`.

## Request body

```jsonc
{
  "question": "string",                 // REQUIRED, non-empty, <= 4000 chars
  "conversation": [                     // optional, <= 50 turns
    { "role": "user" | "assistant",     // REQUIRED per turn (case-insensitive)
      "content": "string",              // REQUIRED, <= 8000 chars
      "name": "string" }                // optional display label (not weighted)
  ],
  "context": [                          // optional, <= 50 sources; total <= 256 KiB*
    { "source_id": "github:issue:123",  // REQUIRED, non-empty (echoed in citations)
      "content": "string",              // REQUIRED (the snippet text)
      "source_type": "closed_issue",    // optional, free-form taxonomy (a hint)
      "title": "string",                // optional
      "url": "https://…",               // optional
      "version": "2.4",                 // optional (for version-aware answers)
      "trust": "verified" | "community" | "unverified" }  // optional; if set, must be one of these
  ]
}
```

\* Context bytes are bounded by `NOGENT_TRIAGE_MAX_CONTEXT_BYTES` (default
262144). The whole request body is bounded by `NOGENT_MAX_BODY_BYTES` (default
2 MiB). Unknown JSON fields are ignored.

Notes:
- `role` is the dialogue position only (`user` = asker, `assistant` = a prior
  reply). Speaker authority is **not** a role — express it via `context.trust`.
- The model is instructed to weight sources `verified > community > unverified`.

## Response `200` (`application/json`)

```jsonc
{
  "answer": "string",
  "confidence": 0.0,                    // float 0.0–1.0 (model self-rating)
  "classification":                     // exactly one of:
    "usage_question" | "bug_report" | "feature_request" |
    "documentation_gap" | "duplicate" | "off_topic" | "needs_more_info",
  "needs_human": false,                 // bool — your approval gate
  "suggested_action":                   // exactly one of:
    "none" | "answer_directly" | "triage_to_human" | "create_ticket" | "link_duplicate",
  "citations": ["github:issue:123"],       // subset of your context source_ids (hallucinated ids dropped)
  "missing_information": ["string"],       // <= 10 items, each <= 300 chars
  "possible_duplicates": ["github:issue:123"] // subset of your context source_ids
}
```

**Fallback:** if the model responds but its output fails canary/shape/enum
validation, you still get a `200` with a fixed body — a generic
"couldn't produce a schema-conforming answer" `answer`, `confidence: 0.0`,
`classification: "needs_more_info"`, `needs_human: true`,
`suggested_action: "triage_to_human"`, empty arrays. **A `200` never means
"trust blindly" — always read `needs_human`.**

Reliability guarantee: `citations` and `possible_duplicates` are filtered to
`source_id`s you actually supplied, so a hallucinated id can never reach you.

## Status codes

| Code | Meaning |
|------|---------|
| `200` | Decision JSON (real answer **or** the fixed fallback — check `needs_human`) |
| `400` | Invalid JSON, empty `question`, bad `role`, bad `trust`, or a source missing `source_id` |
| `401` | Missing/invalid bearer token |
| `404` | Endpoint disabled (no secret configured) or wrong path |
| `413` | `question`/turn too long, >50 turns, >50 sources, or context over the byte cap |
| `429` | Worker pool saturated — back off and retry |
| `502` | Upstream (Gemini) call failed |

## Confidence caveat

`confidence` is the model's self-rating and is not well-calibrated in absolute
terms. Use it as a **relative** threshold and rely on `needs_human` as the
primary gate.

## Example

```bash
curl -s https://HOST/api/triage \
  -H "Authorization: Bearer $NOGENT_TRIAGE_SECRET" \
  -H 'content-type: application/json' \
  -d '{
    "question": "How do I allow writes to /tmp?",
    "conversation": [{"role":"user","content":"tried --allow, still denied"}],
    "context": [
      {"source_id":"docs:fs","source_type":"doc","title":"FS policy",
       "url":"https://nono.sh/docs/fs","version":"2.4",
       "content":"Use --allow-write /tmp to grant write access.","trust":"verified"}
    ]
  }'
```

## Recommended collector logic

```text
resp = POST /api/triage(...)
switch resp.status:
  429 -> exponential backoff, retry
  4xx -> log + drop (client bug: fix payload)
  502 -> retry a few times, else escalate to human
  200 ->
    if resp.needs_human or resp.confidence < THRESHOLD (e.g. 0.8)
       or resp.classification in {needs_more_info, off_topic}:
         route to maintainer approval
    else:
         reply resp.answer, resolving links from resp.citations
    if resp.classification == "duplicate": surface resp.possible_duplicates
```

Map your Discord thread to `conversation` (asker → `user`, prior bot/staff
replies → `assistant`) and your retrieval hits to `context` with a stable
`source_id` per item — that identifier is what comes back in `citations`.
