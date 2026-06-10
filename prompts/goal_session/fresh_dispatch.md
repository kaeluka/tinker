## Fresh dispatch

Whenever you can decompose a sub-task, default to dispatching it to a fresh sub-session of your own goal rather than doing it inline. Each sub-session stays focused on its task, which preserves quality and keeps your own context tight.

```
<@{your-goal-id}|label>
sub-task description
</@{your-goal-id}|label>
```

Replace `{your-goal-id}` with your actual goal ID and `label` with a short correlation tag. The label is optional — use an empty label `<@{your-goal-id}|>` if you don't need correlation. Each fresh sub-session receives the same startup context as you — including fresh-dispatch capability — and replies via `<@{your-goal-id}>your reply</@{your-goal-id}>`. A sub-session may itself dispatch further sub-sessions, acting as a coordinator: it remains reachable for replies until the batch ends. Each dispatched sub-task must be a genuine decomposition — narrower than the problem the dispatcher received, with the LLM deciding when a task is atomic enough not to decompose further.

**Each envelope must be self-contained.** The sub-session only sees the text inside the tags — not your surrounding reply, not earlier turns. Write every fresh dispatch as if the sub-session starts cold: include all the context it needs to complete the task without referring to "above" or "earlier."

**The @-envelope is the only permitted means of spawning sub-sessions.** Tool-level agent-spawning APIs offered by the LLM backend — such as an `Agent` tool — must not be used. Such calls bypass the harness entirely: results are not delivered to the dispatcher, sub-sessions spawned that way are invisible to the goals pane and the event log, and batch accounting breaks. Use the @-envelope exclusively.