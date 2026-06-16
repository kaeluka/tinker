## Fresh dispatch

Whenever you can decompose a sub-task, default to dispatching it to a fresh sub-session of your own goal rather than doing it inline. Each sub-session stays focused on its task, which preserves quality and keeps your own context tight.

Use the `spawn_session` tool:

```
spawn_session(subgoal="...", label="...")
```

The `subgoal` parameter is the task description — it must be self-contained: the sub-session only sees this text, not your surrounding reply or earlier turns, so include all the context it needs to complete the task without referring to "above" or "earlier." The optional `label` parameter is a short correlation tag the sub-session echoes back in its reply envelope (e.g. `"investigate-auth"`). Pass an empty string or omit it when you don't need correlation.

The tool fires the sub-session immediately during your turn (in-turn, not at end-of-turn) and returns the new sub-session id and label. The sub-session runs concurrently with your current turn — you keep reasoning while it works. A sub-session may itself dispatch further sub-sessions, acting as a coordinator: it remains reachable for replies until the batch ends. The schema exposes no target parameter — the new sub-session is always of *your own goal*. To dispatch to a different goal, use `send_message` instead. Each dispatched sub-task must be a genuine decomposition — narrower than the problem you received, with the LLM deciding when a task is atomic enough not to decompose further.

Sub-sessions reply to you via `<@{your-goal-id}>your reply</@{your-goal-id}>` envelopes. The reply mechanism is unchanged from before this tool existed.

The `<@{your-goal-id}|label>...</@{your-goal-id}|label>` envelope syntax continues to work as a transitional form. Prefer the `spawn_session` tool when available: the tool call cannot be malformed, fires in-turn, and surfaces the sub-session id in the model-visible result. The envelope form will be retired once the tool is established (same trajectory as `send_message` / `<@id>...</@id>` envelopes).

**Other tool-level agent-spawning APIs offered by the LLM backend — such as an `Agent` tool — must not be used.** Such calls bypass the harness entirely: results are not delivered to the dispatcher, sub-sessions spawned that way are invisible to the goals pane and the event log, and batch accounting breaks. Use the `spawn_session` tool (or the `<@{your-goal-id}|label>...</@{your-goal-id}|label>` envelope during the transition) exclusively.
