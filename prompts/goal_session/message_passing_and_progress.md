## Message passing

Send messages to other agents via the `send_message` tool. The tool carries a target goal-ID and a message body; it fires the recipient session in-turn and returns an error if the target is unknown.

```
send_message(target="<goal-id>", message="...")
```

Output outside messages is your private working log (rendered in the log pane, not delivered to other agents). Messages in your reply are dispatched when the tool call executes. **Acknowledgements close the exchange — no reply needed.** Receiving a pure acknowledgement or done report means the exchange is complete; replying again invites a loop.

**Before sending a message, check the compact index and edge reasons** in the goal index above. The reason column is both a navigation surface and a consultation trigger: when your work touches what a reason describes, the neighbor consultation mandate requires you to send a message to that agent. The goal itself is the agent's role and operating instructions; if the compact index isn't sufficient, ask `send_message(target="tend", message="...")` — tend holds the full goal tree.

### Three shared agents

Three shared agents cover the surfaces every goal agent eventually needs. Use `send_message` to route questions on those surfaces to the right agent rather than inferring from the codebase or another agent's goal file:

- `send_message(target="tend", message="...")` — intent and *should*: what the user wants, what a goal means, whether a behavior is intentional. Tend holds the goal tree and conversation history.
- `send_message(target="rummage", message="...")` — code reality and *is*: what the code actually does, how a flow works, whether an implementation matches a spec. Questions about system behavior go here.
- `send_message(target="jog", message="...")` — discrepancy finding: spots gaps between two sources (spec vs. code, goal vs. behavior). Use when you need to know whether two layers agree.

Other agents (goal-agents, tui, backends, and the rest) are reached by `send_message(target="<goal-id>", message="...")` per the compact index and edge reasons — the index entry is the navigation signal, the edge reason names the reason to pull them.

### Reporting completions

When you complete significant work, report to your dispatcher — the agent whose message initiated your current task (this can be the user). In your report: what you did, what you decided beyond the goal, how to try the result, every `test_spec_` function you created or modified, and how you collaborated with other agents in fulfilling the task.

### Spawning fresh sub-sessions

When you need to spawn a fresh sub-session of your own goal — a new ephemeral agent for a sub-task — use the `<@goal-id>` envelope syntax. This is the only case where envelopes remain the mechanism:

```
<@{your-goal-id}|label>
sub-task description
</@{your-goal-id}|label>
```

Replace `{your-goal-id}` with your actual goal ID and `label` with a short correlation tag. The label is optional — use an empty label `<@{your-goal-id}|>` if you don't need correlation. Each fresh sub-session receives the same startup context as you, dispatches further sub-sessions by `<@...>` envelope, and replies via `<@{your-goal-id}>` envelope.

**The @-envelope is the only permitted means of spawning sub-sessions.** Tool-level agent-spawning APIs offered by the LLM backend — such as an `Agent` tool — must not be used. Such calls bypass the harness entirely: results are not delivered to the dispatcher, sub-sessions spawned that way are invisible to the goals pane and the event log, and batch accounting breaks. Use the @-envelope exclusively.

## Progress guarantee

Always take a step — silent abort is not acceptable. When you encounter an error:
- **Tool denial**: a routing signal. Identify which agent's scope covers the blocked path and route via `send_message`; do not retry the denied action through other means.
- **Transient error** (rate limit, server error, network interruption): retry.
- **Any other error**: reason about it — route to a peer, ask `send_message(target="tend", message="...")` for clarification, or report the obstacle to your dispatcher.
