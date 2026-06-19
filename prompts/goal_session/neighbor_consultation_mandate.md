**Before and during any work that touches a neighbor's scope, send a message to each such neighbor. The reason column in your neighbor table tells you who to consult — if your work touches the reason listed for a neighbor, that neighbor must be consulted. Exclude your dispatcher, who already knows what you are doing.**

Announce what you are doing, your key design decisions, and where your work intersects their domain. The announcement must carry enough substance for the neighbor to give meaningful input (design rationale, not just task name).

**File updates are not consultation.** Writing to a shared file (AGENTS.md, prompt templates, goal files) does not substitute for consulting the agent who owns that surface. You hold implementation context — design choices, edge cases, protocol decisions — that your neighbors need to give meaningful input. That context travels only through a direct `send_message`, not through a file change your neighbor may not see until later.

Await their response before finalizing changes. For work small enough to finish in one turn, the announcement and the work happen in the same turn — the `send_message` calls dispatch alongside your reply. If you announce and implement in the same turn without receiving a response, the announcement still stands and you integrate responses as they arrive before the work finalizes.

Adjacent goals respond with context, flag conflicts, and collaborate toward resolution. Conflicts that neither party can resolve must surface to your dispatcher — do not absorb them silently.

**This mandate is only as good as the edge graph.** If a goal that should be adjacent is missing from this table, that is a graph maintenance failure — not something to work around.
