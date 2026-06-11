# Native backend — design notes

## Attribution legend

`[U]` = stated/asked/confirmed by user. `[C]` = suggested or proposed by Claude. Unattributed = neutral framing of the problem space, not a commitment.

## Goal

- [U] Four drivers, jointly: (1) better enforcement of what agents can do, (2) less complexity — only one backend will remain, (3) no external CLI dependency, (4) easier setup.

## Constraints

- [U] Controlling what agents can do via the CLI backends has been a hassle; a dedicated backend should solve that.
- [U] The claude CLI backend will eventually be dropped (June 15 2026 billing change: `claude -p` moves off subscription onto a capped Agent SDK credit pool at API rates).
- [U] The opencode agent-file mechanism (temp `tinker-*.md` files written into `.opencode/agent/`) is terrible; the native backend must not need anything like it.

## Open questions raised, not yet answered

- [C → awaiting U, parked — post-CLI-drop] When the future spawn-subagent tool lands, how does it square with the `fresh-agents` goal, which currently mandates the @-envelope as the *only* spawning path and explicitly excludes native agent-creation? (Likely: goal rewritten because a harness-implemented tool preserves routing/visibility — but that's unconfirmed.)
- [C → awaiting U] Session-store design: where conversation history lives, persistence across restarts, compaction strategy.
- [C → awaiting U] Timeline for deleting `claude.rs`.

## Design space

### Axis: bash containment — decided

- [U] Goal agents (rummage/jog/goal sessions) get plain, full bash access. No allowlist, no worktree, no OS sandbox for now.
- [U] Possible later addition: an LLM judge on tool calls, "a bit like auto mode". Not in scope now.
- [C] Ruled-out-for-now options: command allowlist, worktree isolation, sandbox-exec (all [C]-proposed, deferred by [U]).

### Axis: enforcement mechanism — open

- [C] Native tool loop with in-process `ToolPolicy` (allow/deny per tool call, path predicates in Rust). Deny surfaces as a `tool_result` error the model adapts to in-loop.
- [C] Middle path, half-heartedly defended: per-invocation CLI flags (`--allowedTools` etc.) + opencode agent-file `permission:` frontmatter. Two dialects, ongoing maintenance.
- Caveat [C]: bash is the hole in the fence — path policies are airtight only for no-bash roles.

### Axis: rollout staging — decided (exit criteria still open)

- [U] End state decided: native is the sole backend; opencode and claude both go.
- [U] Migration is staged (option b): native lands behind a flag, runs alongside opencode for a shakedown period; CLIs deleted once trusted.
- [U] All work happens on a new branch.
- [U] All goal files must be updated consistently. [C] Affected (grep): `backends`, `model-config`, `prompt-storage`, `agent-liveness`, `goal-agents`, `startup-args`, `fresh-agents` — backend mentions, the six config slots, `--claude` flag ownership, error-reinjection template consumers.
- [C] Phase 1: native OpenRouter runner for tend + cleanup (no-bash roles, fully enforceable).
- [C] Phase 2: goal agents (rummage/jog/sessions) move native once a bash containment story is picked.
- [C] ~~Cleanup reshaped to completion (LLM returns cleaned file content; tinker writes + re-scans).~~ **Retracted by [U]: cleanup is a goal session and must not be treated differently — it keeps the same agentic shape and tools as goal sessions.** Scheduler — described in `SCHEDULER_MODEL` comment but has no call sites today — would be native-first if built.

### Axis: tool set — decided for v1

- [U] v1 tools, exactly six: `bash`, `read`, `write`, `edit`, `glob`, `grep`. Nothing else. Turn ends when the model replies without tool calls.
- [U] Future, after the CLI backends are dropped: probably add a **message tool** (peer @-messages as a first-class tool), and after that a **spawn-subagent tool**.
- [C] Flagged: the spawn tool is in tension with the current `fresh-agents` goal text (see open questions).

### Axis: provider — open

- [U] OpenRouter API key as the credential (premise of the idea).
- [C] Anthropic models remain available via OpenRouter; dropping claude.rs loses no model access.

## Decisions made

- [U] Goal is multi-driver: enforcement + single-backend simplicity + no CLI dependency + easier setup. No single driver dominates by default.
- [U] End state: native backend is the only backend.
- [U] Goal agents get plain full bash; LLM-judge gating is a possible later layer, out of scope now.
- [U] Per-role tool matrix confirmed: tend = no bash, read anywhere, write/edit restricted to `.tinker/goals/**`; goal sessions (rummage/jog/sessions, **including cleanup**) = full bash, unrestricted read/write.
- [U] Cleanup is a goal session — not treated differently, not reshaped into a completion (corrects earlier [C] proposal).
- [U] Session history is in-memory only; tinker restart = fresh sessions (matches current behavior). No on-disk transcript store.
- [U] Context overflow v1: fail visibly, goal gets a fresh session (option a). Design should leave room to migrate to summarize/compact (option c) later. Sliding-window (b) ruled out.
- [U] Staged migration on a new branch: flag-gated native runner alongside opencode; CLIs deleted once trusted; all goal files updated consistently.
- [U] Config & setup: config.toml gains a `[native]` section (high/mid/low tier slots, openrouter model IDs); API key only via `OPENROUTER_API_KEY` env var, never in config files; `[claude]`/`[opencode]` sections live through the staged period and die with the CLIs. Setup = one env var + run.
- [U] v1 tool set: bash, read, write, edit, glob, grep — nothing else. Message tool and spawn-subagent tool are post-CLI-drop future work, in that order.
- [U] Flag is `--native`; CLI backends are deleted once `--native` mode works well — judgment call, no formal checklist or time box.
- [C, inferred from staging decision] During the staged period the native runner implements the existing `OpenCodeRunner` trait (inventing its own session ids), so it slots into the five runner positions unchanged.

## Things explicitly NOT assumed

- That migration happens in phases (end state is decided; the path is not).
- That compaction (option c) will ever actually be built — v1 only keeps the door open.
- That the scheduler role will be built.
- That `claude.rs` is deleted immediately on June 15.

## Where we ended up

A native Rust backend talking directly to OpenRouter (`OPENROUTER_API_KEY` env var; `[native]` config.toml section with high/mid/low slots), replacing both CLI backends as tinker's sole backend. It owns the agent loop: six tools (bash, read, write, edit, glob, grep), per-role policy enforced in-process — tend gets no bash and writes only under `.tinker/goals/**`; all goal sessions including cleanup get full unrestricted tools. Sessions are in-memory only; context overflow fails visibly and restarts fresh (compaction is a kept-open later option). Built on a new branch behind `--native`, running alongside opencode until it works well, then `opencode.rs`/`claude.rs` and their config sections are deleted, with all backend-referencing goal files updated consistently. Post-deletion roadmap: message tool, then spawn-subagent tool (requires revisiting `fresh-agents`).

## Conversation log

- [U] asked whether a native (non-CLI) backend taking an OpenRouter key makes sense.
- [C] laid out tradeoffs: CLIs provide the agentic harness (tool loop, sessions, compaction); native gains control/reliability; proposed three options incl. native-for-completion-shaped-roles.
- [U] asked which roles are completion-shaped. [C] traced call sites: cleanup (after reshape) and the not-yet-built scheduler; corrected earlier overstatement — tier ≠ shape, low-tier goal sessions are still tool-using agents.
- [U] revealed core motivation: controlling agent capabilities has been a hassle; a dedicated backend would solve that. [C] diagnosed root cause (CLI permissions built for interactive approval; `--dangerously-skip-permissions` + prompt-level enforcement) and flagged the bash caveat.
- [U] stated claude CLI will eventually be dropped due to June 15 `-p` change. [C] verified: billing split, not feature restriction; concluded cost objection to native is gone and tinker trends single-backend.
- [U] invoked /spec-it for the native backend.
- [U] set goal: enforcement + less complexity (single backend end state) + no CLI dep + easier setup; called the opencode agent-file hack terrible. Resolved goal + end-state questions.
- [U] decided bash containment: plain full bash for goal agents; maybe an LLM judge later (auto-mode-like). [C]'s allowlist/worktree/sandbox options deferred.
- [U] confirmed tool matrix; tend's row exactly right.
- [U] corrected [C]: cleanup is a goal session, gets the same treatment as other goal sessions — the cleanup-as-completion idea is retracted.
- [U] chose in-memory-only session store (option a).
- [U] chose fail-visibly + fresh session for context overflow, with a later path to compaction (a → c).
- [U] chose staged migration (b): new branch, flag-gated native runner, CLIs deleted once trusted; goal files updated consistently. [C] greped the affected goal files into the staging axis.
- [U] confirmed [C]'s config/setup proposal ([native] section + OPENROUTER_API_KEY env var).
- [U] confirmed six-tool set for v1; future post-CLI-drop additions: message tool, then spawn-subagent tool. [C] flagged tension with `fresh-agents` goal's envelope-only rule.
- [U] exit criterion: delete CLIs once `--native` mode works well. Open questions for current scope now empty; [C] added "Where we ended up".
- [U] asked to build it on a new branch. [C] built v1 on `native-backend`: `src/native.rs` (reqwest + hand-rolled loop — no agent framework, per [U] question about langchain), `[native]` config slots, `--native` wiring + startup guards, goal files/README/AGENTS.md updated. 344 tests pass (2 failures pre-exist on main).
