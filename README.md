# The Pristine Agent Harness (Engine)

Pristine (`1p`) is an agentic harness that I am actively developing for the purpsoses of education and exploring some
ideas. Properly, Pristine is an agentic harness engine, since its goal is to support experimentation in harness design
by providing composable features that users may configure.

Rather than prioritizing a TUI-first development environment, Pristine is going to build a stable API/ABI, which
multiple clients may leverage. As of right now, that interface is evolving.

Pristine will be extensible to work with different models, although I will prioritize the ones I work with the most.

Pristine will support deep configuration over the agentic loop itself, allowing customization of history, default
prompts, skills, memory.

Pristine will support running multiple agents in parallel, and enable them to interact with each other.


## Running the chat client from source

Pristine ships with a bare-bones demonstration chat client to interact with the harness layer. This chat client is NOT
intended for use as a daily-driver coding agent, although you could try to eat your salad with a forklift.

From any directory you want the agent to operate in, run:

```sh
just --justfile /path/to/pristine/justfile chat
```

Or add a shell alias for convenience:

```sh
alias 1p-chat='just --justfile /path/to/pristine/justfile chat'
```

Then invoke it from a project directory:

```sh
cd ~/my-project
1p-chat
```
