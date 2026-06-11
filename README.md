# Tinker

**Status:** Tinker is early-stage and experimental. Bugs are expected.
Features are in heavy flux. This is a design study; work is actively ongoing.

---

## The model

Every goal in tinker is a running agent. Each goal owns a piece of the
spec and the code that implements it; when work begins, the goal session
activates, reads its own spec, and writes directly to its files. Goals
communicate by dispatching `<@goal-id>...</@goal-id>` tag envelopes to each other —
consulting peers, reporting findings, delegating checks — without any
central coordinator. This is what separates tinker from a conventional
coding assistant: the specification is not a passive document but a live
network of agents that enforce their own constraints.

---

You tell an AI what to build. The AI writes code. Now your codebase has two
kinds of content mixed together: the things you actually wanted, and the
things the AI guessed on its own. Over time you cannot tell them apart.

Most AI coding tools work this way. They are fast at writing code but bad at
saying why. Every new session adds more guesses. What starts as a shortcut
turns into a codebase nobody understands.

Tinker is a different kind of tool. It keeps the human intent separate from
the AI-generated code. The codebase stays readable on your terms — not as a
side effect of whatever the AI happened to produce.

---

## Intent, not output

The problem with most AI coding tools is not that they write bad code. It is
that they write everything into the same place: source files. A function
exists because you asked for it, or because the AI thought you might need it,
or because the AI made a mistake in an earlier session and wrote a workaround
you never saw. Once the session is over, nobody can tell which is which.

Tinker fixes this with a layer called **goals**. A goal is a written
instruction that says what matters and why. It lives in its own file,
separate from the code. Goals survive restarts. They track cleanly in
version control. Every line of generated code has a goal that explains why
it exists.

Goals are kept deliberately sparse. Each line must be anchored by something
outside the implementation — a decision you actually made, an external
constraint, or a hard lesson from a previous attempt. Nothing goes in just
because it seemed like a good idea. That sparseness matters because goals do
double duty: they are the standing record of your intent AND the prompt the
goal session reads when it starts work. A cluttered goal gives the agent a
cluttered target.

The result: a codebase that is readable because it is the result of choices
you made, not a pile of guesses the AI left behind.

---

## How it works

Tinker organizes work across three layers: your **intent** — what you want
to build, before it has precise language — the **goal** that writes it down
explicitly, and the **code** that implements the goal. Each layer is
answerable to the one before it.

The system runs in both directions.

**Forward — building.** You bring the judgment. You spot the friction, feel
what fits and what does not, and decide when a result is good enough. The
tool is named after what you do: tinker.

**Tend** (the conversational agent) draws out what you want through a
back-and-forth: it proposes a framing, you accept, reject, or push back, it
refines, and over several rounds something crystallizes that resonates. The
resolution does not come from either side alone — it is constructed through
the exchange.

One consequence of this is that the process must be hard for you. The
friction of finding language for something that does not yet have language
is exactly the work that generates user-anchored material. When tend makes
the process easy — proposing ready-made framings, filling gaps on your
behalf — you have handed over the only contribution only you can make. What
enters the goal layer then is tend's content wearing user-vetted provenance.
The friction is not a cost to minimize; it is the signal that the right work
is happening.

A second consequence is that you cannot be replaced in this loop. A second
language model standing in for you has no real stake in the outcome, no
contact with the problem outside the system, no continuity across sessions.
It cannot supply the external bounding that terminates the refinement loop
at a useful point. You are not in the loop as a courtesy — you are the
termination condition.

Once the goal says what you mean, tend dispatches a **goal session** — a
persistent agent dedicated to that goal — which reads the spec, writes the
code, and reports back to tend when done. Multiple goal sessions can be
active at once; you select any goal in the goal list to see its session log.

The loop continues after a goal is written. A sparse goal is implemented by
a goal session; you try the result; observations feed back into the goal;
the goal updates; the session re-runs paying attention to the delta. A
sparse goal converges toward correctness through iteration, not by getting
the spec right the first time.

Tend does not guess. When it cannot resolve something from the rules alone,
it stops and asks. When the framing is wrong, tend names the shift
explicitly before continuing. It never silently redirects.

**Backward — checking.** The same four agents cover the return path, looking
for gaps between adjacent layers.

Two axes organize them into a system rather than a list:

- **Producers vs. skeptics.** Tend and goal sessions move forward and treat
  the goal as authoritative — they build from it. Rummage and jog move
  backward and treat the goal as signal-with-noise — the gap under
  investigation may be in the goal itself. When a skeptic finds a
  contradiction, it surfaces the finding to tend. Tend owns the verdict —
  code drift or intent gap — and routes any repair from there.

- **Observable vs. drawn-out inputs.** Goal sessions and rummage work with
  directly observable inputs — the written goal and the running code
  respectively. They operate without user involvement. Tend and jog work
  with inputs that cannot be read directly: tacit intent can only be drawn
  out through dialogue. That is why they are the human-coupled agents.

**Rummage** is the precision backstop — the agent other goals route to when
they need code reality, technical validation, or a counterexample. Every claim
rummage receives is treated as a hypothesis; execution is the only thing that
closes it. Language models cannot reliably answer questions about running
behavior from static analysis alone — reading files produces inference, not
ground truth. Reading code is only for forming a hypothesis; every hypothesis
must then be tested by running the code — scratch tests, traced outputs,
exercised code paths. If
execution fails, rummage withholds the answer rather than substituting a guess.
Rummage is preauthorized to run code without asking permission — it does not
stop to check before executing. Every finding it reports names exactly what was
executed to produce it. It does not decide what behavior should be; that is
tend's call. When rummage finds behavior that appears to contradict a goal, it
surfaces the finding to tend. Tend owns the verdict on whether the gap is a
code drift or an intent gap, and routes any repair from there.

**Jog** checks for gaps between any two layers. It builds a set of things in
each source — what the spec says, what the code does — by sending read-only
queries to peer agents (tend for the spec layer, rummage for the code layer).
It then compares the two sets step by step.

Two kinds of problems emerge. A **coverage gap** (forward direction) is
something in the code that the spec does not account for — a bug-finding
lens, because unaccounted behavior is unintended behavior until proven
otherwise. A **point of interest** (backward direction) is something in the
spec with no traceable origin in the code — a place where the design has
not landed yet.

Jog documents findings in a discrepancy log. It does not commission fixes.

The cycle is closed: everything built forward is a candidate for scrutiny
backward. Code that cannot be explained by its goal, or a goal that no
longer matches your intent, is a failure the backward pass is there to
catch.

---

## Agents

Tend, rummage, and jog are the three conversational agents — the interactive
layer you speak with directly. What "How it works" does not make obvious is
that they consult each other — directly, without you in the middle.

When rummage needs to know what you intended, it sends a message to tend.
When tend needs to know what the code actually does, it sends a message to
rummage. When jog wants to read the spec, it asks tend; when it wants to read
the code, it asks rummage. These exchanges appear in the session logs. You can
see them; you do not manage them.

Switch to tend, rummage, or jog with the matching slash command. Every goal
session is also addressable the same way — `/` followed by the goal's id.
The prompt line shows which is active.

**Tend is the right place to start any new conversation.** Rummage and jog
get pulled in as the work calls for them.

---

## Architecture

Every goal is an agent — an addressable session that persists for the
project's lifetime. When a goal needs to distribute parallel work, it can
dispatch ephemeral sub-sessions of itself — each one a full participant in
the message-passing protocol. Any sub-session can itself act as a coordinator,
dispatching further sub-sessions to any depth. The session view nests them
accurately under their immediate dispatcher at every level; when a dispatcher
labels a sub-session, that label appears in place of a generated name, so you
can tell at a glance what each piece of parallel work is doing. When the batch
ends, all sub-sessions at every depth are retired.

Any agent — persistent or ephemeral — can send a message to any other by name.
There is no central dispatcher. Delivery to any recognised agent is guaranteed —
the harness never silently drops a message. When a goal session finds a gap it
cannot resolve, it messages rummage. When rummage needs intent context, it
messages tend. When jog wants to read the spec, it messages tend. The exchange
resolves without you brokering it.

This is an actor model applied to specification documents. The goals are not
passive files waiting for a human to notice conflicts. They are live sessions
that can enforce their own constraints and consult peers to resolve ambiguity.

The consequence for you: you stay informed through tend's reports and the
session logs, without ever having to read the code behind them. Cognitive debt
stays low because the goals — which you write and approve — are the whole
story.

---

## Who holds what

Tinker follows a simple rule about state:

- **You hold the *should***. What you want to build. The decisions you make.
  The goals you approve.
- **Tinker holds the *is***. The current state of every file, every build
  artifact, every cache, every side effect of anything that has happened so
  far.

Tinker never asks you to remember what a previous session did. It checks by
looking — running the code, reading the files. It never substitutes a guess
for a check.

When Tinker tells you about its internals, it uses words you already know.
It does not throw file paths or function names at you. You only see the parts
you need to operate the tool: the binary path, the flags, the environment
variables. Everything behind that is Tinker's job to keep straight.

---

## Installation

Build from source:

```
cargo build --release
```

Add the binary to your PATH:

```
export PATH="$PATH:/path/to/tinker/target/release"
```

Tinker ships with a set of packaged goals — built-in goals that apply to
every project. To make them available, symlink the `packaged-goals/`
directory into `~/.tinker`:

```
ln -s /path/to/tinker/packaged-goals ~/.tinker/packaged-goals
```

Tinker merges goal directories from your home directory down to your project.
The symlink puts the packaged goals in the ancestor position, so any project
inherits them without copying files.

Run tinker from your project directory:

```
tinker
```

You get a three-pane terminal: a conversation pane where you talk to the
active agent (tend, rummage, or jog), a list of your goals with the full
text of the selected goal below it, and a log of whatever goal session is
currently running. The prompt line in the conversation pane shows which
agent is active. All text areas scroll with the mouse wheel. New content
follows the bottom of the view unless you scroll up.

Three backends are available:
- **Default** — uses opencode with configurable models.
- **Claude** — pass `--claude` to use the Claude CLI directly.
- **Native** — pass `--native` to talk to OpenRouter directly, with no CLI
  in between (requires `OPENROUTER_API_KEY`). The agent tool loop runs
  in-process, which lets tinker enforce per-role capability boundaries in
  code (tend cannot run bash and writes only under `.tinker/goals/`).
  Experimental; intended to replace both CLI backends.

---

## About this document

This README is written by Tinker itself. It reads its own goals and
synthesizes the narrative you see here. It is as authoritative as the goals
it reflects. As Tinker's understanding of itself changes, this document
changes with it.
