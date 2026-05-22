
## 5. The Decision Log: Preventing convention engines from cycling

Because an LLM is a convention engine, it will deterministically arrive at similar architectural conclusions given the same starting constraints. If we test a conventional idea (e.g., "use a HOW block in the goal file to give instructions") and realize it is a mistake, we need a way to prevent the engine from re-deriving that same mistake next week.

Currently, we put these negative constraints into the `DECISIONS` block of the relevant Goal (e.g., "Do not use a HOW block"). But this pollutes the active intent file with tombstones of bad ideas. 

If we remove the `HOW` block entirely (the "sparse vs dense" debate applied to intent), we achieve a cleaner spec, but we lose the historical context of *why* we don't do it. The agent might autonomously re-invent the `HOW` block later because the structural vacuum invites it to apply standard conventions again.

We need to explore a structural pattern for a **Decision Log** or an "Anti-Pattern Cache." This would be a place where we document "tried and discarded ideas" and the reasoning behind discarding them. It gives the convention engine the historical context needed to avoid cycling, without cluttering the active `WHAT/WHY/SCOPE` of the standing goals.

Open questions for this project:
- Does the Decision Log live inside the `.toml` file alongside the active intent, or is it a separate artifact?
- If it's separate, how do we ensure it stays in context when the relevant goal is executed?
