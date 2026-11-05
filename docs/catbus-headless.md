# Running `catbus-agent` headless

`catbus-agent` is normally a tab you look at. It also answers one question and
exits, which is what makes it usable from a shell script, a build step or a git
hook — anywhere you want a model's judgement but have nowhere to draw a TUI.

This page is about that mode only.

## The one command

```sh
printf 'Summarise this in one sentence: the deploy failed because the disk filled.\n' |
    catbus-agent --prompt-file - --no-tui --once --ansi=false
```

That prints the answer on stdout and exits. The parts that matter:

| flag | why |
|---|---|
| `--prompt-file -` | read the prompt from stdin. A path (`--prompt-file ask.txt`) works too |
| `--no-tui` | no in-tab REPL: nothing tries to draw, so it is safe to run detached |
| `--once` | leave when the answer is given rather than staying to serve a socket |
| `--ansi=false` | no escape sequences in the reply, so the output is safe to capture |
| `--timeout 180` | optional, seconds. Defaults to 180 |

`--prompt-file` is what makes this a one-liner. Without it the caller has to open
the agent's UNIX socket and speak its protocol, which in practice means another
language to build the request — and a script that needs a language runtime to ask
a question is a script people stop running.

## The I/O contract

Worth relying on, because scripts do:

- **stdout is the answer, and nothing else.** Reasoning, warnings and logs go to
  stderr. `answer=$(...)` gets the answer.
- **exit 0** means an answer was produced. **exit 1** means it was not, with a
  `catbus-agent: …` line on stderr saying why.
- a failure never prints a partial answer to stdout, so an empty capture and exit
  1 are the same fact.
- a one-shot run that overruns `--timeout` exits 1 with
  `the prompt did not finish within 180s`, rather than being killed silently.

There is no streaming in this mode: the answer arrives whole.

## Giving it an identity

By default the agent is told it is Claude Code, which is untrue on any endpoint
that is not Anthropic's and makes it answer in the wrong shape for a mechanical
job. `--identity-file` replaces the whole system prompt:

```sh
cat > /tmp/answer-with-a-verdict.md <<'EOF'
---
AllowedTools:
---
You answer with a verdict and nothing else.

Reply with exactly one of:

    VERDICT: clean
    VERDICT: not clean
EOF

printf 'Is this sentence polite? Yes it is.\n' |
    catbus-agent --prompt-file - --no-tui --once --ansi=false \
        --identity-file /tmp/answer-with-a-verdict.md
```

The front matter is optional and takes three keys:

| key | |
|---|---|
| `AllowedTools` | which tools the file's author permits |
| `AllowedHosts` | which hosts `SSH` may reach |
| `AllowedJumpHosts` | which jump hosts it may route through |

Anything else in the header is an **error**, not a warning. The header carries
permission directives, so a typo like `AllowedTool:` would otherwise leave the
full tool set in place and say nothing at all — the opposite of what the author
wrote. Blank lines and `#` comments are fine.

### Declaring no tools

```
---
AllowedTools:
---
```

An empty list means **no tools at all**: the agent cannot read, write, run or
fetch. That is the right declaration for a job whose answer comes entirely from
the prompt — a reviewer, a classifier, a rewriter.

The distinction matters in three ways, and they are not the same:

| header | outcome |
|---|---|
| `AllowedTools:` (empty) | no tools, deliberately |
| `AllowedTools: NotATool` | error: unknown tool name |
| no `AllowedTools` at all | no restriction — whatever `--tools-config` allowed |

The third is the one to be careful about. An **omitted** key narrows nothing, so
a launcher's `--tools-config` decides. Only an explicit empty list means none. If
the run must not act, say so in the file and check with `--tools-config`.

## A worked example: a git hook that asks for a diff review

This is the arrangement that motivated the mode: a pre-push hook that refuses to
send a diff carrying a person's name, a hostname or a key.

```sh
#!/bin/sh
# .githooks/pre-push — review what is about to be published.
set -eu

base=$(git rev-parse origin/main)
diff=$(git diff "$base"...HEAD --no-color --unified=0)

# Nothing added, nothing to review.
[ -n "$diff" ] || exit 0

answer=$(printf '%s\n\n%s\n' \
    'Review these added lines. Report a person, hostname, private path or key;
say VERDICT: clean when there is none.' \
    "$diff" |
    catbus-agent --prompt-file - --no-tui --once --ansi=false --timeout 180 \
        --identity-file .catbus/diff-reviewer.md \
        --tools-config .catbus/no-tools.json) || {
    echo 'review: the agent did not answer, refusing to push' >&2
    exit 1
}

case $answer in
*'VERDICT: clean'*) exit 0 ;;
*)
    printf 'review: the agent found something:\n%s\n' "$answer" >&2
    exit 1
    ;;
esac
```

with

```sh
# .catbus/no-tools.json — belt and braces beside the identity file.
{"allow": []}

# .catbus/diff-reviewer.md
---
# Answer from the prompt alone: nothing to read, nowhere to ask.
AllowedTools:
---
You review a diff before it is published.
Report a person's name, a server hostname, a private absolute path or an API key.
Do not comment on style or design.
End with `VERDICT: clean` or `VERDICT: with findings`.
```

Three habits from that example, each learned the hard way:

**Ask a lone reviewer for evidence, not an impression.** Made to decide directly,
an agent answered `VERDICT: clean` on a diff it had called a leak a minute earlier.
Made to work a short checklist and write each line — `CHECK names: none`, then a
verdict — it stopped doing that. If a verdict can be wrong in the expensive
direction, require the reasoning that produces it, and treat a verdict without it
as no answer.

**Do not send what you have not scanned.** A model call discloses its input. The
deterministic checks — emails, private paths, this machine's hostname, key
material, a deny list — belong *before* the call, so the obvious leaks never leave
the machine. Use the agent for what a matcher cannot see: whether a name is
attached to a real person.

**Match on a substring, not the whole line.** The same answer arrives as
`VERDICT: clean` one run and `**VERDICT: clean**` the next — models decorate. The
`case` above matches anywhere in the answer for that reason; `[ "$answer" =
'VERDICT: clean' ]` would refuse a clean review at random, which is the failure
mode that gets a hook deleted.

**A missing agent is not a pass.** `|| exit 1` above is deliberate. A review tool
that silently approves when it is unavailable is worse than no review, because
nobody notices.

## Troubleshooting

**`unknown key 'Author'`** — older builds skipped unrecognised header keys.
Remove the line: only `AllowedTools`, `AllowedHosts` and `AllowedJumpHosts` do
anything, and a key that is read by nothing still reads as configuration.

**It hangs** — check `--timeout` was given and that `--once` is present. Without
`--once` the process stays up to serve its socket, which is correct for a tab and
wrong for a script.

**Empty output, exit 1** — read stderr. A named identity file that cannot be read
or holds no prompt is an error rather than a fallback to the built-in prompt, so a
typo is loud.

**It asks a question and then times out** — one-shot mode has no channel to
answer on, so a mid-turn question can only wait. Give the agent no tools and there
is nothing to ask about.
