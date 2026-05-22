# Testing Standard — Design Notes

These notes elicit a new section to add to the `coding-standards` goal
(`~/.tinker/goals/coding-standards.toml`) covering what to test.

## Attribution legend
- `[U]` = stated, asked, or confirmed by the user
- `[C]` = suggested or proposed by Claude
- Unattributed = neutral framing of the problem space, not a commitment

---

## Goal

`[U]` Define what makes a good test in a regime where AI writes the code and humans rarely read the implementation. This is a new section appended to the existing `coding-standards` goal — the existing standard already makes code testable; this one says what to test and what good tests look like under different economics.

---

## Constraints

_None yet._

---

## Open questions raised, not yet answered

_(no open question — back to discussion)_

---

## Design space

_(Retracted: I prematurely dumped a 10-item strategy catalog with categorization. The user pushed back — we're discussing strategies informally before pinning anything down.)_

---

## Decisions made

- `[U]` Tests are a **guardrail for the AI** that writes the code.
- `[U]` The guardrail covers two things: **spec-compliance** and **security**.
- `[U]` AI writes the tests, derived from the goal description (the spec).
- `[U]` Tests come in two categories:
  - **spec-level tests** — directly trace to the goal description. MUST be marked as spec-level via a test-name prefix or comment.
  - **other tests** — AI's discretion (internal correctness, regression, sanity). Unmarked.
- `[U]` **Security** is tricky and project-specific. It lives in a separate `security.md` at the repo root that names the security context and lists threats; each threat lists mitigation strategies.

---

## Things explicitly NOT assumed

- No assumption about test framework (the existing standard doesn't mandate one).
- No assumption about coverage targets.
- No assumption about whether this section dictates test STYLE or test SUBJECT (what to test vs. how to write the test).

---

## Conversation log

- `[U]` Requested a new section for the coding standard about what to test.
- `[U]` Existing standard makes testing easy but doesn't define what makes a good test; AI writes the code, humans rarely read it — that shifts the economics.
- `[U]` Tests are a guardrail for the AI to build spec-compliant and secure code.
- `[U]` AI writes tests from the spec. Spec-level tests must be marked (name prefix or comment); other tests are AI's discretion.
- `[U]` Security is project-specific; lives in `security.md` at repo root listing threats and mitigation strategies per threat.
- `[C]` Proposed strategy catalog for security.md (type encoding, static analysis, spec-level security tests, fuzz/property tests, sanitization, runtime assertions, sandboxing, dep audit, review, logging+alerting). **Retracted — too much too fast; user wants to discuss first.**
