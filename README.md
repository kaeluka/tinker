# Tinker

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

The result: a codebase that is readable because it is the result of choices
you made, not a pile of guesses the AI left behind.

---

## How it works

Tinker has two roles that work together.

**You** bring the judgment. You know what feels right and what does not. You
spot the friction, the things that could work better. You decide when a
design is good enough and when it needs another pass. The tool is named after
what you do: you tinker.

**Tinker** (the conversational agent) handles the rules. It reads your
goals, checks them against each other, finds the gaps and conflicts, and
ensures the system that emerges follows them. It does not write code itself —
that is what goal sessions do. It is good at following instructions and bad at
guessing what you meant. When it cannot figure something out from the rules
alone, it stops and asks you. It does not make things up. When the framing
itself is wrong — when the current question is not the right question —
tinker names the shift explicitly before continuing. It never silently
redirects.

Two more agents live in the same conversation pane. Both face the opposite
direction from tinker: instead of building forward from intent to code, they
look for gaps.

**Rummage** checks whether the code does what the goal says. When something
needs explaining — a bug, surprising output, behavior you do not trust, code
you are about to change and want to understand first — you bring in rummage.
It investigates: reads what is there, writes scratch tests, and traces the
problem from its symptom backward to the conditions that caused it. The
deliverable is a document: an explanation, an assessment, or groundwork for
the next step.

**Jog** checks whether the goal still says what you mean. Goals are written
at a point in time; your understanding moves. You open jog by naming a topic
in your own words — "jog me on logging" — and jog holds a conversation with
you. It asks you to articulate what you know, then probes the why, without
first telling you what the goal says. If what you say matches what is
written, nothing changes. If it does not, jog hands the edit off to tinker, which applies it and shows
you what changed — without running another interview, because jog's
conversation already did that work.

You switch between the three agents with `/tinker`, `/rummage`, and `/jog`.
The prompt line always shows which is active.

The two of you work in rounds. You say what you want. Tinker writes it
down as a goal and reads it back to you. You correct it. It reads it back
again. Once the goal says what you mean, tinker hands it to a **goal
session** — a focused agent that reads the goal, writes the code, and
reports back what it did. Only one goal session runs at a time, so you
always know what is happening.

Goals are kept deliberately sparse. Each line in a goal must be anchored by
something outside the implementation — a decision you actually made, an
external constraint, or a hard lesson from a previous attempt. Nothing
goes in just because it seemed like a good idea. That sparseness matters
because goals do double duty: they are the standing record of your intent
AND the prompt the goal session reads when it starts work. A cluttered
goal gives the agent a cluttered target. A sparse goal gives the iteration
loop room to converge — the session implements what the goal says, you try
the result, and if something is missing or wrong, that gap drives the next
update to the goal rather than staying hidden.

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

## Running Tinker

```
cargo run
```

You get a three-pane terminal: a conversation pane where you talk to the
active agent (tinker, rummage, or jog), a list of your goals with the full
text of the selected goal below it, and a log of whatever goal session is
currently running. The prompt line in the conversation pane shows which
agent is active. All text areas scroll with the mouse wheel. New content
follows the bottom of the view unless you scroll up.

Two backends are available:
- **Default** — uses opencode with configurable models.
- **Claude** — pass `--claude` to use the Claude CLI directly.

---

## About this document

This README is written by Tinker itself. It reads its own goals and
synthesizes the narrative you see here. The goals are the source of truth;
this document is a reflection of them. As Tinker's understanding of itself
changes, this document changes with it.