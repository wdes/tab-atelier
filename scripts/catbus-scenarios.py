#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Give a built catbus-agent a set of real tasks and report what it did.

This is how the agent's features are verified end to end: not "does the unit test
pass" but "hand it a job and see what it makes of it". Each scenario runs the real
binary against a scenario-specific working directory, is handed one task over its
socket, and is scored on artifacts — the files that appeared, the tool calls the
transcript recorded, the task list on disk — rather than on what it claims.

    scripts/catbus-scenarios.py target/release/catbus-agent
    scripts/catbus-scenarios.py target/release/catbus-agent tasks_tool spawn_subagent

**This makes real API calls and costs real money.** It is not part of `gate.sh`, not
run by CI, and should not be. Ten scenarios is ten agent sessions.

It uses the operator's own `$HOME`, so the agent resolves its relay exactly as a tab
does, and varies only the cwd. That is deliberate on two counts: copying a credential
into a throwaway HOME so an agent can authenticate is indistinguishable from
handling one improperly, and it would also be less faithful — a tab's agent reads the
real config, so testing against anything else tests a different program. Nothing here
handles a token.

Scoring reaches the transcript and the `Tasks` list by *path*, using the same cwd
escaping catbus uses. Both of those were learned the hard way: walking
`~/.claude/projects` picks up every session on the machine, including the operator's
interactive ones, whose tool calls then look like this agent's — which mistook a
working `AllowedTools` for a broken one. Searches are the enemy in a verifier.

Results are written to /tmp/catbus-scenarios/results.json, and a summary table is
printed. Exit status is zero only when every scenario passed.
"""
import json, os, shutil, socket, subprocess, sys, time

BIN = sys.argv[1] if len(sys.argv) > 1 else "/mnt/Dev/@wdes/tab-atelier/target/debug/catbus-agent"
WANTED = sys.argv[2:]
ROOT = "/tmp/catbus-scenarios"
REAL_CONFIG = os.path.expanduser("~/.config/tab-atelier/preferences.json")
PER_TASK_TIMEOUT = 300


def log(msg):
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


def escape_cwd(cwd):
    """catbus's own rule for turning a cwd into a directory name.

    Every non-alphanumeric character becomes `-`, not just the slashes — see
    `session::escape_cwd`. Getting this wrong looks exactly like an agent that ran
    no tools: the transcript is read from a directory that does not exist.
    """
    return "".join(c if c.isalnum() and c.isascii() else "-" for c in cwd)


def read_tasks(home, cwd):
    """The tasks the agent recorded for `cwd`, read from the tool's own path.

    Built the way `tools::tasks::list_path_in` builds it — the cwd with every
    non-alphanumeric character replaced by `-` — rather than searching a state
    directory for something that looks like a task list. A search over the real
    `~/.local/state` finds unrelated JSON and reports the last one it parsed, which
    is how a working Tasks tool looked like it had saved nothing.
    """
    path = os.path.join(
        home, ".local", "state", "tab-atelier", "agent-tasks", f"{escape_cwd(cwd)}.json"
    )
    try:
        return json.load(open(path)).get("tasks", [])
    except Exception:
        return []


def agent_home(name):
    """The real HOME, and a scenario-specific working directory.

    Deliberately *not* a throwaway HOME with a copy of `preferences.json`: copying a
    credential so an agent can authenticate is indistinguishable from exfiltrating
    one, and it is also less faithful — a tab's agent resolves its relay from the
    real config, so testing against anything else tests a different program. Here the
    agent finds its own endpoint exactly as it does in a tab, and this script never
    handles the token at all.

    Only the cwd varies per scenario. That is enough to keep them apart: the
    transcript is keyed by cwd, and so is the `Tasks` list.
    """
    home = os.path.expanduser("~")
    cwd = os.path.join(ROOT, name, "cwd")
    shutil.rmtree(os.path.join(ROOT, name), ignore_errors=True)
    os.makedirs(cwd, exist_ok=True)
    return home, cwd


class Agent:
    """One agent process, driven over its socket."""

    def __init__(self, name, home, cwd, extra=(), prompt_file=None):
        self.name = name
        self.home = home
        self.cwd = cwd
        self.sock = os.path.join(ROOT, name, "agent.sock")
        self.log = os.path.join(ROOT, name, "agent.log")
        env = dict(os.environ, HOME=home)
        # The relay is resolved from the real preferences, which is the point. Only a
        # *stale override* is cleared, so a shell that exported one cannot silently
        # redirect every scenario at some other endpoint.
        env.pop("CATBUS_RELAY_URL", None)
        env.pop("CATBUS_RELAY_TOKEN", None)
        args = [BIN, "--cwd", cwd, "--new-session", "--no-tui", "--name", name,
                "--socket", self.sock]
        if prompt_file:
            args += ["--identity-file", prompt_file]
        args += list(extra)
        self.proc = subprocess.Popen(args, env=env, stdout=open(self.log, "w"),
                                     stderr=subprocess.STDOUT, text=True)
        deadline = time.time() + 60
        while time.time() < deadline:
            if os.path.exists(self.sock):
                break
            if self.proc.poll() is not None:
                raise RuntimeError(f"agent exited at startup:\n{open(self.log).read()[-1200:]}")
            time.sleep(0.2)
        else:
            raise RuntimeError("agent never opened its socket")

    def ask(self, text, timeout=PER_TASK_TIMEOUT):
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.settimeout(timeout)
        s.connect(self.sock)
        f = s.makefile("rw", buffering=1)
        f.readline()  # handshake
        f.write(json.dumps({"kind": "prompt", "text": text}) + "\n")
        reply = None
        started = time.time()
        while time.time() - started < timeout:
            line = f.readline()
            if not line:
                break
            ev = json.loads(line)
            if ev.get("kind") in ("done", "error"):
                reply = ev
                break
        s.close()
        return reply

    def transcript(self):
        """The session transcript this agent wrote, as parsed entries.

        Scoped to the project directory for this scenario's own cwd. It has to be:
        the agent runs against the real `$HOME`, so walking all of
        `~/.claude/projects` would read every session on the machine — including the
        operator's interactive ones, whose tools (`Agent`, `TaskCreate`,
        `ToolSearch`) then appear as if this agent had called them. That mistook a
        working agent for a broken one.
        """
        base = os.path.join(self.home, ".claude", "projects", escape_cwd(self.cwd))
        found = []
        if os.path.isdir(base):
            for f in os.listdir(base):
                if f.endswith(".jsonl"):
                    found.append(os.path.join(base, f))
        entries = []
        for path in found:
            for line in open(path, errors="replace"):
                line = line.strip()
                if not line:
                    continue
                try:
                    entries.append(json.loads(line))
                except Exception:
                    pass
        return entries

    def tool_calls(self):
        """(name, short input) for every tool the agent ran."""
        calls = []
        for e in self.transcript():
            c = (e.get("message") or {}).get("content")
            if not isinstance(c, list):
                continue
            for b in c:
                if b.get("type") == "tool_use":
                    calls.append((b.get("name"), json.dumps(b.get("input"))[:160]))
        return calls

    def tool_results(self):
        out = []
        for e in self.transcript():
            c = (e.get("message") or {}).get("content")
            if not isinstance(c, list):
                continue
            for b in c:
                if b.get("type") == "tool_result":
                    text = b.get("content")
                    text = text if isinstance(text, str) else json.dumps(text)
                    out.append({"is_error": b.get("is_error"), "text": text[:300]})
        return out

    def stop(self):
        self.proc.terminate()
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait(timeout=5)


# ---------------------------------------------------------------- scenarios

SCENARIOS = {}


def scenario(fn):
    SCENARIOS[fn.__name__] = fn
    return fn


@scenario
def tool_census():
    """What the model believes it has, against what the client offered.

    The client logs `offering N tools`, but the relay reshapes the array on the way
    through — this route's upstream is typed and ignores what it does not know. A
    tool the client offers and the model never sees is a dead feature that looks
    present in every log line, so the only honest check is to ask the model.
    """
    home, cwd = agent_home("tool_census")
    a = Agent("tool_census", home, cwd)
    try:
        reply = a.ask(
            "List the exact names of every function or tool available to you, one per line, and "
            "nothing else. Do not call any of them — just name them."
        )
        answer = (reply or {}).get("text", "")
        offered = ""
        for line in open(a.log, errors="replace"):
            if "offering" in line and "tools" in line:
                offered = line.strip()
        named = [w.strip(" \t*-.,") for w in answer.replace("\n", ",").split(",")]
        named = [n for n in named if n]
        return {
            "asked": "name every tool you have",
            "client_offered": offered,
            "model_said": answer[:600],
            "model_named_count": len(named),
            "pass": any(t in answer for t in ("Spawn", "Tasks")),
            "why": "" if any(t in answer for t in ("Spawn", "Tasks"))
                   else "the model did not name Spawn or Tasks",
        }
    finally:
        a.stop()


@scenario
def write_and_shell():
    """The plain tool loop: write a file, then read it back with a shell."""
    home, cwd = agent_home("write_and_shell")
    a = Agent("write_and_shell", home, cwd)
    try:
        reply = a.ask(
            "Create a file named notes.txt in your working directory containing exactly the "
            "line: catbus works. Then run a shell command to print what it contains, so the "
            "file is proven rather than assumed."
        )
        target = os.path.join(cwd, "notes.txt")
        body = open(target).read() if os.path.exists(target) else ""
        calls = [n for n, _ in a.tool_calls()]
        return {
            "asked": "write notes.txt, then prove it with a shell command",
            "reply_kind": (reply or {}).get("kind"),
            "answer": (reply or {}).get("text", "")[:300],
            "reasoning_present": bool((reply or {}).get("reasoning")),
            "tools_used": calls,
            "artifact": repr(body),
            "pass": body.strip() == "catbus works" and "Bash" in calls,
            "why": "" if body.strip() == "catbus works" and "Bash" in calls
                   else f"file={body.strip()!r} tools={calls}",
        }
    finally:
        a.stop()


@scenario
def tasks_tool():
    """The new Tasks tool, driven the way the tool description says to use it."""
    home, cwd = agent_home("tasks_tool")
    a = Agent("tasks_tool", home, cwd)
    try:
        reply = a.ask(
            "You have a Tasks tool. Plan this three-step job in it, then carry it out: "
            "1) list the files in your working directory, 2) create done.txt saying finished. "
            "Use the tool to record each step as you go, and to mark each one done. "
            "Finish by listing the tasks."
        )
        calls = a.tool_calls()
        task_calls = [(n, i) for n, i in calls if n == "Tasks"]
        actions = []
        for _n, i in task_calls:
            try:
                actions.append(json.loads(i).get("action"))
            except Exception:
                pass
        saved = read_tasks(home, cwd)
        done = os.path.exists(os.path.join(cwd, "done.txt"))
        passed = "add" in actions and "done" in actions and bool(saved) and done
        return {
            "asked": "plan 3 steps with Tasks, do them, mark each done",
            "reply_kind": (reply or {}).get("kind"),
            "answer": (reply or {}).get("text", "")[:300],
            "tasks_tool_calls": len(task_calls),
            "actions": actions,
            "tasks_on_disk": len(saved),
            "final_statuses": [t.get("status") for t in saved],
            "artifact_done_txt": done,
            "pass": passed,
            "why": "" if passed else f"actions={actions} saved={len(saved)} done={done}",
        }
    finally:
        a.stop()


@scenario
def tasks_tool_explicit():
    """The same job as `tasks_tool`, with wording that cannot be misread.

    The first version said "use the tool to record each step", and the model went
    hunting for a task *binary* — eight `Bash` calls running `command -v tasks`,
    `ls /usr/bin | grep todo`, `find / -iname '*task*'`. Naming it as a registered
    function, and saying not to look for a command, separates a tool that does not
    work from a sentence that can be read two ways.
    """
    home, cwd = agent_home("tasks_tool_explicit")
    a = Agent("tasks_tool_explicit", home, cwd)
    try:
        reply = a.ask(
            "Call your Tasks function — it is one of the functions registered for you, not a "
            "shell command, so do not use Bash to look for it. Do this: 1) call Tasks with "
            "action=add to record the step 'create done.txt'; 2) create done.txt containing "
            "the word finished; 3) call Tasks with action=done for that task; 4) call Tasks "
            "with action=list and tell me what it says."
        )
        calls = a.tool_calls()
        task_calls = [i for n, i in calls if n == "Tasks"]
        actions = []
        for i in task_calls:
            try:
                actions.append(json.loads(i).get("action"))
            except Exception:
                pass
        saved = read_tasks(home, cwd)
        done = os.path.exists(os.path.join(cwd, "done.txt"))
        passed = "add" in actions and "done" in actions and bool(saved) and done
        return {
            "asked": "call the Tasks function explicitly (add/done/list)",
            "reply_kind": (reply or {}).get("kind"),
            "answer": (reply or {}).get("text", "")[:300],
            "tasks_calls": len(task_calls),
            "actions": actions,
            "tasks_on_disk": len(saved),
            "final_statuses": [t.get("status") for t in saved],
            "artifact_done_txt": done,
            "pass": passed,
            "why": "" if passed else f"actions={actions} saved={len(saved)} done={done}",
        }
    finally:
        a.stop()


@scenario
def spawn_subagent():
    """The new Spawn tool: an agent starting another agent."""
    home, cwd = agent_home("spawn_subagent")
    a = Agent("spawn_subagent", home, cwd)
    try:
        reply = a.ask(
            "Use your Spawn tool to start a sub-agent. Give it exactly this task: "
            "'create a file named child.txt in the working directory containing the word "
            "spawned, then report the absolute path you used'. Then tell me whether the file "
            "exists and what the sub-agent said."
        )
        calls = a.tool_calls()
        spawned = [i for n, i in calls if n == "Spawn"]
        child = os.path.join(cwd, "child.txt")
        body = open(child).read() if os.path.exists(child) else ""
        # The child runs with --once and its own socket; nothing of it should remain.
        stray = []
        for pid in os.listdir("/proc"):
            if not pid.isdigit():
                continue
            try:
                cmd = open(f"/proc/{pid}/cmdline").read().replace("\0", " ")
            except Exception:
                continue
            if "catbus-sub-" in cmd:
                stray.append(cmd[:80])
        passed = bool(spawned) and body.strip() == "spawned" and not stray
        return {
            "asked": "spawn a sub-agent to write child.txt",
            "reply_kind": (reply or {}).get("kind"),
            "answer": (reply or {}).get("text", "")[:400],
            "spawn_calls": len(spawned),
            "artifact_child_txt": repr(body),
            "leftover_subagents": stray,
            "pass": passed,
            "why": "" if passed else f"spawns={len(spawned)} child={body.strip()!r} stray={stray}",
        }
    finally:
        a.stop()


@scenario
def plan_mode_refuses():
    """Plan mode must propose rather than act."""
    home, cwd = agent_home("plan_mode_refuses")
    a = Agent("plan_mode_refuses", home, cwd, extra=["--gate", "plan"])
    try:
        reply = a.ask("Create a file named planned.txt containing the word yes.")
        exists = os.path.exists(os.path.join(cwd, "planned.txt"))
        calls = [n for n, _ in a.tool_calls()]
        results = a.tool_results()
        refused = any(r.get("is_error") for r in results)
        passed = (not exists) and (refused or "Write" not in calls)
        return {
            "asked": "write a file while in plan mode",
            "reply_kind": (reply or {}).get("kind"),
            "answer": (reply or {}).get("text", "")[:300],
            "tools_used": calls,
            "file_written": exists,
            "refusal_seen": refused,
            "pass": passed,
            "why": "" if passed else f"file={exists} tools={calls}",
        }
    finally:
        a.stop()


@scenario
def auto_mode_records():
    """Auto mode must judge the write and record what it decided."""
    home, cwd = agent_home("auto_mode_records")
    a = Agent("auto_mode_records", home, cwd, extra=["--gate", "auto"])
    try:
        reply = a.ask("Create a file named vetted.txt containing the word checked.")
        exists = os.path.exists(os.path.join(cwd, "vetted.txt"))
        results = a.tool_results()
        record = [r["text"] for r in results if "auto checked" in r["text"]]
        log = open(a.log).read()
        judged = "auto checked" in log
        passed = exists and bool(record) and judged
        return {
            "asked": "write a file while in auto mode",
            "reply_kind": (reply or {}).get("kind"),
            "file_written": exists,
            "record_in_tool_result": record[0][:200] if record else "",
            "record_in_log": [l for l in log.splitlines() if "auto checked" in l][:1],
            "pass": passed,
            "why": "" if passed else f"file={exists} record={bool(record)} log={judged}",
        }
    finally:
        a.stop()


@scenario
def identity_and_allowed_tools():
    """A prompt file that replaces the system prompt and caps the tools."""
    home, cwd = agent_home("identity_allowed")
    prompt = os.path.join(ROOT, "identity_allowed", "identity.md")
    with open(prompt, "w") as f:
        f.write("---\nAllowedTools: Read\n---\nYou are a terse filing clerk. "
                "Answer in one short sentence, and never use a tool you were not given.\n")
    a = Agent("identity_allowed", home, cwd, prompt_file=prompt)
    try:
        reply = a.ask(
            "Create a file called denied.txt containing the word nope, then tell me in one "
            "sentence what happened."
        )
        exists = os.path.exists(os.path.join(cwd, "denied.txt"))
        calls = [n for n, _ in a.tool_calls()]
        # What the agent asked the relay for, as its own request body cannot be read
        # here; the prompt's effect is visible in behaviour instead.
        passed = not exists and "Bash" not in calls
        return {
            "asked": "write a file, with only Read allowed",
            "reply_kind": (reply or {}).get("kind"),
            "answer": (reply or {}).get("text", "")[:300],
            "tools_used": calls,
            "file_written": exists,
            "pass": passed,
            "why": "" if passed else f"file={exists} tools={calls}",
        }
    finally:
        a.stop()


@scenario
def resume_claude_transcript():
    """The operator's actual failure: resume a Claude Code session and keep going.

    Uses a real transcript, copied into the throwaway HOME under the cwd it was
    recorded for, because the 400 came from that content: assistant turns whose
    only block is thinking.
    """
    src = "/home/williamdes/.claude/projects/-tmp/2414fb43-419b-42d1-ada4-3a82398e24d8.jsonl"
    # A fresh id, so this cannot overwrite the operator's own session: it is a copy
    # of that session under a name of its own, and it is removed afterwards.
    sid = "scn00000-0000-4000-8000-000000000001"
    home, cwd = agent_home("resume_claude_transcript")
    cwd = "/tmp"
    proj = os.path.join(home, ".claude", "projects", cwd.replace("/", "-"))
    os.makedirs(proj, exist_ok=True)
    shutil.copy(src, os.path.join(proj, f"{sid}.jsonl"))
    a = Agent("resume_claude_transcript", home, cwd, extra=["--resume", sid])
    try:
        reply = a.ask("Continue: in one sentence, what were we working on?")
        kind = (reply or {}).get("kind")
        text = (reply or {}).get("text", "")
        passed = kind == "done" and bool(text.strip())
        return {
            "asked": "resume a real Claude Code session that 400d before",
            "reply_kind": kind,
            "answer": text[:300],
            "error": (reply or {}).get("message", "")[:200],
            "pass": passed,
            "why": "" if passed else f"kind={kind} err={(reply or {}).get('message','')[:120]}",
        }
    finally:
        a.stop()
        # Leave the real project directory as it was found: the copy and everything
        # the run wrote beside it.
        for path in os.listdir(proj):
            if path.startswith(sid):
                os.remove(os.path.join(proj, path))


@scenario
def tool_loop_depth():
    """A task that needs several rounds of tool use, to probe the loop and the cap."""
    home, cwd = agent_home("tool_loop_depth")
    a = Agent("tool_loop_depth", home, cwd)
    try:
        reply = a.ask(
            "Do this with tools, one step at a time: create five files named step1.txt "
            "through step5.txt, each containing its own number. Then list the directory so "
            "the five files are proven to exist, and tell me how many there are."
        )
        made = [f"step{i}.txt" for i in range(1, 6)]
        present = [f for f in made if os.path.exists(os.path.join(cwd, f))]
        calls = [n for n, _ in a.tool_calls()]
        passed = len(present) == 5 and calls.count("Write") >= 5
        return {
            "asked": "create five files one at a time, then prove it with a listing",
            "reply_kind": (reply or {}).get("kind"),
            "answer": (reply or {}).get("text", "")[:300],
            "files_present": len(present),
            "writes": calls.count("Write"),
            "bash": calls.count("Bash"),
            "pass": passed,
            "why": "" if passed else f"present={present} writes={calls.count('Write')}",
        }
    finally:
        a.stop()


def main():
    names = WANTED or list(SCENARIOS)
    os.makedirs(ROOT, exist_ok=True)
    results = []
    for name in names:
        log(f"=== {name} ===")
        started = time.time()
        try:
            r = SCENARIOS[name]()
        except Exception as e:
            r = {"pass": False, "why": f"harness error: {type(e).__name__}: {e}"}
        r["scenario"] = name
        r["seconds"] = round(time.time() - started, 1)
        results.append(r)
        log(f"    {'PASS' if r.get('pass') else 'FAIL'}  {r.get('why','')}")

    with open(os.path.join(ROOT, "results.json"), "w") as f:
        json.dump(results, f, indent=2)

    print("\n" + "=" * 100)
    print(f"{'scenario':<28} {'result':<7} {'secs':>5}  note")
    print("-" * 100)
    for r in results:
        print(f"{r['scenario']:<28} {'PASS' if r.get('pass') else 'FAIL':<7} {r.get('seconds',0):>5}  {r.get('why','')[:50]}")
    passed = sum(1 for r in results if r.get("pass"))
    print("-" * 100)
    print(f"{passed}/{len(results)} scenarios passed")
    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
