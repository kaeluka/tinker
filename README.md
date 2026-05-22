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

The **orchestrator** handles the rules. It reads your goals, checks them
against each other, finds conflicts, and writes code that follows them. It
is good at following instructions and bad at guessing what you meant. When
it cannot figure something out from the rules alone, it stops and asks you.
It does not make things up.

The two of you work in rounds. You say what you want. The orchestrator
writes it down as a goal and reads it back to you. You correct it. It reads
it back again. Once the goal says what you mean, the orchestrator hands it
to a **goal session** — a focused agent that reads the goal, writes the
code, and reports back what it did. Only one goal session runs at a time, so
you always know what is happening.

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

You get a three-pane terminal: a conversation with the orchestrator, a list
of your goals, and a log of whatever goal session is currently running. All
text areas scroll with the mouse wheel. New content follows the bottom of
the view unless you scroll up.

Two backends are available:
- **Default** — uses opencode with configurable models.
- **Claude** — pass `--claude` to use the Claude CLI directly.

---

## About this document

This README is written by Tinker itself. It reads its own goals and
synthesizes the narrative you see here. The goals are the source of truth;
this document is a reflection of them. As Tinker's understanding of itself
changes, this document changes with it.