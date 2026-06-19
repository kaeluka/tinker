# Tinker

**Status:** Tinker is early-stage and experimental. Bugs are expected.
Features are in heavy flux. This is a design study; work is actively
ongoing.

---

## The model

Every goal in tinker is an agent you reach by name — and the
agents reach each other the same way you reach them. Specifications
are not passive documents: they are live participants in the same
address space you sit in. This is what separates tinker from a
conventional coding assistant, where one session owns everything and
you have to read source to know what the AI did.

---

## Installation

Tinker calls language models through any endpoint that speaks the
OpenAI chat-completions protocol. The defaults target OpenRouter — a
service that gives you access to many models behind one endpoint. You
can also point each tier at any other compatible endpoint, including
local servers like ollama, llama.cpp, or vLLM that run on your own
machine and don't require authentication.

**1. Build from source:**

```
cargo build --release
```

**2. Add the binary to your PATH:**

```
export PATH="$PATH:/path/to/tinker/target/release"
```

**3. Configure model tiers.** Tinker reads `.tinker/config.toml` in
your working directory. On first run, tinker writes a starter template
that documents every available setting; the template is fully commented
out so the file changes nothing on its own. The loaded defaults match
what the template shows — if OpenRouter is what you want, no edits
are needed; the only remaining step is auth.

The file has three sub-tables, one per tier — `high` serves tend,
rummage, and jog (the strongest tier), `mid` serves goal sessions,
`low` serves the cheapest background work. Each sub-table takes two
fields:

```toml
[native.high]
endpoint = "<chat-completions URL>"
model    = "<model identifier>"

[native.mid]
endpoint = "<chat-completions URL>"
model    = "<model identifier>"

[native.low]
endpoint = "<chat-completions URL>"
model    = "<model identifier>"
```

Either field may be omitted independently — absent fields fall back to
the OpenRouter defaults. To use a different provider, edit the lines
you want to override and restart tinker.

**Auth.** Each tier authenticates from its own environment variable:

```
export TINKER_HIGH_API_KEY="your-key"
export TINKER_MID_API_KEY="your-key"
export TINKER_LOW_API_KEY="your-key"
```

If a variable is unset or empty, tinker sends no `Authorization`
header for that tier — which is what local model servers expect. The
convention is generic on purpose: it works with any OpenAI-protocol
endpoint, not just OpenRouter. The config file itself never holds
credentials.

**4. (Optional) Customize the universal goals.** The packaged goals
shipped with tinker — the agent machinery tinker is built on — are
part of the binary itself. To customize any of them without
rebuilding, symlink the source tree's packaged-goals directory into
your home directory:

```
ln -s /path/to/tinker/.tinker/goals/packaged-goals ~/.tinker/goals/packaged-goals
```

The ancestor-merge picks them up; your edits shadow the binary's
defaults.

**5. Run tinker from your project directory:**

```
tinker
```

You get a three-pane terminal: a conversation pane where you talk to
the active agent, a list of your goals with the full text of the
selected goal below it, and a log of whatever goal session is currently
running. The prompt line shows which agent is active.

---

## Agents

Three conversational agents share the interactive layer you speak with
directly: tend, rummage, and jog.

- **Tend** is the entry point. Tend conducts the interview that turns
  what you want into a written goal, then dispatches goal sessions to
  implement it. Tend is also the agent every other agent consults when
  they need to know what you intended. Start every new conversation
  with tend.

- **Rummage** is the precision backstop. When anything needs to know
  what the code actually does — or to verify a claim — rummage runs
  the code and reports. Rummage never infers behavior from reading
  files alone.

- **Jog** checks for gaps between layers. When a goal is implemented,
  jog reads the spec (through tend) and the code (through rummage) and
  reports what is covered, what is missing, and what has no traceable
  origin in either layer.

The three agents consult each other directly. When rummage needs to
know what you intended, it sends a message to tend. When tend needs to
know what the code actually does, it sends a message to rummage. When
jog wants to read the spec, it asks tend; when it wants to read the
code, it asks rummage. These exchanges appear in the session logs. You
can see them; you do not manage them.

Switch between agents with the matching slash command — `/tend`,
`/rummage`, `/jog`. Every goal session is also addressable the same
way: `/` followed by the goal's id. The prompt line shows which is
active.

---

## Architecture

Every goal in tinker is an addressable agent that can enforce its own
constraints and consult peers. The goals are not passive files waiting
for a human to notice conflicts — they are live sessions that converse
to resolve ambiguity and ensure compliance. This is an actor model
applied to specification documents.

The conversation happens through the `send_message` tool: when one goal
needs another's input, it sends a message to that goal by name. There
is no central dispatcher. Delivery to any recognised agent is
guaranteed; an unknown target fails outright. When a goal needs to
distribute parallel work, it can spawn fresh sub-sessions of itself —
each one a full participant in the protocol, recursing to any depth.

Because the agent loop runs in process, constraints are enforced in
code. Tend is restricted to a narrow write scope and cannot run shell
commands; goal sessions get the full tool set. A goal that oversteps
its boundary hits a code barrier, not a guideline.

The consequence for you: you stay informed about the current feature
set through tend's reports and the session logs, without ever having
to read the code. Cognitive debt stays low because the goals —
which you write and approve — are the whole story. Manual
documentation rots as AI-built projects evolve; this README is itself
derived from the goal tree, so it cannot drift from what tinker
actually is.
