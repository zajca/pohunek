"""Run GitHub composite actions and workflow steps locally (stdlib only).

The harness executes the real `run:` bodies with bash, the way the runner
does (`bash --noprofile --norc -eo pipefail` for `shell: bash`), renders
`${{ }}` expressions against `inputs`, `steps`, `runner` and `env`, and
replaces the third-party actions the pinned cache uses with in-process
fakes: `actions/cache/restore` and `actions/cache/save` share one
`FakeCache` keyed by the exact key and path, as the real service keys an
entry by its key and a version derived from the paths. Network commands
are shims in `shims/` that record each call and fail while the run is
offline, so a hit that still reaches the network fails its scenario.

The YAML reader covers the subset the repository's actions and job steps
use (block mappings and sequences, plain and quoted scalars, `|` and `>`
block scalars, comments); anything else raises instead of guessing.
"""

import io
import json
import os
import re
import shutil
import subprocess
import tarfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
# Upper bound for one step; the slowest real body here is a retried
# install with a one-second backoff.
STEP_TIMEOUT_SECONDS = 60


class UnsupportedYaml(ValueError):
    pass


class StepFailed(AssertionError):
    def __init__(self, step, output):
        super().__init__(f"step {step!r} failed:\n{output}")
        self.step = step
        self.output = output


# ---------------------------------------------------------------- YAML subset


def _strip_comment(rest):
    if rest.startswith(("'", '"')):
        return rest
    match = re.search(r"(?:^|\s)#", rest)
    return rest[: match.start()].rstrip() if match else rest.rstrip()


def _scalar(rest, where):
    rest = _strip_comment(rest.strip())
    if not rest:
        return None
    if rest[0] in "[{&*!%@`":
        raise UnsupportedYaml(f"{where}: unsupported scalar {rest!r}")
    if rest[0] == "'":
        match = re.fullmatch(r"'((?:[^']|'')*)'(?:\s+#.*)?", rest)
        if not match:
            raise UnsupportedYaml(f"{where}: bad single-quoted scalar {rest!r}")
        return match.group(1).replace("''", "'")
    if rest[0] == '"':
        match = re.fullmatch(r'"((?:[^"\\]|\\.)*)"(?:\s+#.*)?', rest)
        if not match:
            raise UnsupportedYaml(f"{where}: bad double-quoted scalar {rest!r}")
        return re.sub(r"\\(.)", lambda m: {"n": "\n", "t": "\t"}.get(m.group(1), m.group(1)), match.group(1))
    return rest


class _Reader:
    def __init__(self, text, name):
        self.lines = text.split("\n")
        self.name = name
        self.i = 0

    def peek(self):
        """(index, indent, content) of the next non-blank, non-comment line."""
        i = self.i
        while i < len(self.lines):
            line = self.lines[i]
            stripped = line.strip()
            if stripped and not stripped.startswith("#"):
                if "\t" in line[: len(line) - len(line.lstrip())]:
                    raise UnsupportedYaml(f"{self.name}:{i + 1}: tab indentation")
                return i, len(line) - len(line.lstrip(" ")), stripped
            i += 1
        return None

    def node(self, min_indent):
        nxt = self.peek()
        if nxt is None or nxt[1] < min_indent:
            return None
        _, indent, content = nxt
        if content == "-" or content.startswith("- "):
            return self.sequence(indent)
        return self.mapping(indent)

    def mapping(self, indent):
        result = {}
        while True:
            nxt = self.peek()
            if nxt is None or nxt[1] < indent:
                return result
            i, found, content = nxt
            if found > indent:
                raise UnsupportedYaml(f"{self.name}:{i + 1}: unexpected indent")
            if content == "-" or content.startswith("- "):
                return result
            match = re.fullmatch(r"([^\s'\"#][^:]*?|'[^']*'|\"[^\"]*\"):(?:\s+(.*))?", content)
            if not match:
                raise UnsupportedYaml(f"{self.name}:{i + 1}: not a mapping entry: {content!r}")
            key = match.group(1).strip("'\"")
            rest = match.group(2) or ""
            self.i = i + 1
            indicator = _strip_comment(rest)
            if indicator in ("|", "|-", "|+", ">", ">-", ">+"):
                result[key] = self.block_scalar(indent, indicator)
            elif indicator == "":
                after = self.peek()
                if after is not None and after[1] == indent and (after[2] == "-" or after[2].startswith("- ")):
                    result[key] = self.sequence(indent)
                else:
                    result[key] = self.node(indent + 1)
            else:
                result[key] = _scalar(rest, f"{self.name}:{i + 1}")

    def sequence(self, indent):
        result = []
        while True:
            nxt = self.peek()
            if nxt is None or nxt[1] != indent or not (nxt[2] == "-" or nxt[2].startswith("- ")):
                return result
            i, _, content = nxt
            item = content[1:].lstrip(" ")
            if not item:
                self.i = i + 1
                result.append(self.node(indent + 1))
                continue
            # Re-read the item as a mapping that starts two columns in.
            pad = len(self.lines[i]) - len(self.lines[i].lstrip(" "))
            offset = pad + 1 + (len(content) - 1 - len(item))
            if re.match(r"([^\s'\"#][^:]*?|'[^']*'|\"[^\"]*\"):(\s|$)", item):
                self.lines[i] = " " * offset + item
                result.append(self.mapping(offset))
            else:
                self.i = i + 1
                result.append(_scalar(item, f"{self.name}:{i + 1}"))

    def block_scalar(self, parent_indent, indicator):
        body = []
        block_indent = None
        while self.i < len(self.lines):
            line = self.lines[self.i]
            if line.strip():
                indent = len(line) - len(line.lstrip(" "))
                if indent <= parent_indent:
                    break
                if block_indent is None:
                    block_indent = indent
                if indent < block_indent:
                    break
                body.append(line[block_indent:])
            else:
                body.append("")
            self.i += 1
        while body and body[-1] == "":
            body.pop()
        if indicator.startswith("|"):
            text = "\n".join(body)
        else:
            text = ""
            for number, line in enumerate(body):
                if line == "":
                    text += "\n"
                elif number and body[number - 1] != "" and not line.startswith(" "):
                    text += " " + line
                else:
                    text += line
        if indicator.endswith("-"):
            return text
        return text + "\n" if text else text


def parse_yaml(text, name="<yaml>"):
    reader = _Reader(text, name)
    value = reader.node(0)
    rest = reader.peek()
    if rest is not None:
        raise UnsupportedYaml(f"{name}:{rest[0] + 1}: unparsed content {rest[2]!r}")
    return value


def workflow_env(text):
    """The workflow-level `env:` mapping of a workflow file."""
    match = re.search(r"(?ms)^env:\n(.*?)(?=^\S)", text)
    if match is None:
        raise AssertionError("the workflow has no top-level env")
    return {key: "" if value is None else value for key, value in parse_yaml(match.group(1), "env").items()}


def job_steps(text, job):
    """The parsed `steps:` of one job of a workflow file."""
    match = re.search(rf"(?ms)^  {re.escape(job)}:\n(.*?)(?=^  [A-Za-z0-9_-]+:\n|\Z)", text)
    if match is None:
        raise AssertionError(f"job {job} not found")
    steps = re.search(r"(?ms)^    steps:\n(.*)", match.group(1))
    if steps is None:
        raise AssertionError(f"job {job} has no steps")
    return parse_yaml(steps.group(1), job)


def iter_step_lists(text, name):
    """Each `steps:` list of a workflow (one per job) or an action file."""
    lines = text.split("\n")
    for number, line in enumerate(lines):
        match = re.fullmatch(r"( *)steps:\s*", line)
        if not match:
            continue
        indent = len(match.group(1))
        block = []
        for follower in lines[number + 1:]:
            stripped = follower.strip()
            if stripped and not stripped.startswith("#"):
                found = len(follower) - len(follower.lstrip(" "))
                if found < indent or (found == indent and not stripped.startswith("- ")):
                    break
            block.append(follower)
        yield parse_yaml("\n".join(block), f"{name}:{number + 1}") or []


def iter_steps(text, name):
    """Every step of every `steps:` list in a workflow or action file."""
    for steps in iter_step_lists(text, name):
        yield from steps


# ---------------------------------------------------------------- expressions

_TOKEN = re.compile(
    r"\s*(?:(?P<str>'(?:[^']|'')*')|(?P<num>-?\d+(?:\.\d+)?)|(?P<op>==|!=|&&|\|\||[!(),])"
    r"|(?P<ident>[A-Za-z_][A-Za-z0-9_-]*(?:\.[A-Za-z_][A-Za-z0-9_-]*)*))"
)


def _truthy(value):
    return value not in (None, False, "", 0)


def _text(value):
    if value is None:
        return ""
    if value is True:
        return "true"
    if value is False:
        return "false"
    return str(value)


class _Expression:
    def __init__(self, source, context):
        self.tokens = []
        position = 0
        source = source.strip()
        while position < len(source):
            match = _TOKEN.match(source, position)
            if match is None or match.end() == position:
                raise UnsupportedYaml(f"cannot tokenize expression {source!r} at {position}")
            kind = match.lastgroup
            self.tokens.append((kind, match.group(kind)))
            position = match.end()
            while position < len(source) and source[position].isspace():
                position += 1
        self.position = 0
        self.context = context
        self.source = source

    def take(self, value=None):
        token = self.tokens[self.position] if self.position < len(self.tokens) else (None, None)
        if value is not None and token[1] != value:
            raise UnsupportedYaml(f"expected {value!r} in {self.source!r}")
        self.position += 1
        return token

    def peek(self):
        return self.tokens[self.position][1] if self.position < len(self.tokens) else None

    def evaluate(self):
        value = self.disjunction()
        if self.position != len(self.tokens):
            raise UnsupportedYaml(f"trailing tokens in {self.source!r}")
        return value

    def disjunction(self):
        value = self.conjunction()
        while self.peek() == "||":
            self.take()
            right = self.conjunction()
            value = value if _truthy(value) else right
        return value

    def conjunction(self):
        value = self.unary()
        while self.peek() == "&&":
            self.take()
            right = self.unary()
            value = right if _truthy(value) else value
        return value

    def unary(self):
        if self.peek() == "!":
            self.take()
            return not _truthy(self.unary())
        return self.comparison()

    def comparison(self):
        value = self.primary()
        if self.peek() in ("==", "!="):
            op = self.take()[1]
            right = self.primary()
            equal = _text(value).lower() == _text(right).lower()
            return equal if op == "==" else not equal
        return value

    def primary(self):
        kind, value = self.take()
        if kind == "str":
            return value[1:-1].replace("''", "'")
        if kind == "num":
            return float(value) if "." in value else int(value)
        if value == "(":
            inner = self.disjunction()
            self.take(")")
            return inner
        if kind != "ident":
            raise UnsupportedYaml(f"unexpected {value!r} in {self.source!r}")
        if value in ("true", "false"):
            return value == "true"
        if value == "null":
            return None
        if self.peek() == "(":
            self.take()
            args = []
            while self.peek() != ")":
                args.append(self.disjunction())
                if self.peek() == ",":
                    self.take()
            self.take(")")
            return self.call(value, args)
        current = self.context
        for part in value.split("."):
            current = current.get(part) if isinstance(current, dict) else None
        return current

    def call(self, name, args):
        if name == "startsWith":
            return _text(args[0]).lower().startswith(_text(args[1]).lower())
        if name == "contains":
            return _text(args[1]).lower() in _text(args[0]).lower()
        if name == "format":
            return re.sub(r"\{(\d+)\}", lambda m: _text(args[int(m.group(1)) + 1]), _text(args[0]))
        if name in ("success", "always"):
            return True
        if name in ("failure", "cancelled"):
            return False
        raise UnsupportedYaml(f"function {name}() is not supported by the harness")


def evaluate(source, context):
    return _Expression(source, context).evaluate()


def render(value, context):
    """Interpolate every `${{ }}` of a string value."""
    if value is None:
        return ""
    return re.sub(r"\$\{\{(.*?)\}\}", lambda m: _text(evaluate(m.group(1), context)), str(value))


def condition(source, context):
    if source is None:
        return True
    source = str(source).strip()
    match = re.fullmatch(r"\$\{\{(.*)\}\}", source, re.S)
    return _truthy(evaluate(match.group(1) if match else source, context))


# ---------------------------------------------------------------- fakes


class FakeCache:
    """The cache service: one entry per (key, path), as actions/cache keys
    an entry by its key and a version derived from the path list."""

    def __init__(self):
        self.entries = {}
        self.restores = []
        self.saves = []
        self.warnings = []

    def restore(self, key, path):
        self.restores.append((key, path))
        blob = self.entries.get((key, path))
        if blob is None:
            return {"cache-primary-key": key}
        target = Path(path)
        target.mkdir(parents=True, exist_ok=True)
        with tarfile.open(fileobj=io.BytesIO(blob)) as archive:
            archive.extractall(target, filter="tar")
        return {"cache-hit": "true", "cache-primary-key": key, "cache-matched-key": key}

    def save(self, key, path):
        source = Path(path)
        if not source.exists():
            self.warnings.append(f"Path Validation Error: {path} does not exist")
            return {}
        if (key, path) in self.entries:
            self.warnings.append(f"Unable to reserve cache with key {key}")
            return {}
        buffer = io.BytesIO()
        try:
            with tarfile.open(fileobj=buffer, mode="w") as archive:
                for child in sorted(source.iterdir()):
                    archive.add(child, arcname=child.name)
        except PermissionError as error:
            self.warnings.append(f"tar failed: {error}")
            return {}
        self.entries[(key, path)] = buffer.getvalue()
        self.saves.append((key, path))
        return {}

    def names(self, key, path):
        with tarfile.open(fileobj=io.BytesIO(self.entries[(key, path)])) as archive:
            return sorted(archive.getnames())

    def replace(self, key, path, files):
        buffer = io.BytesIO()
        with tarfile.open(fileobj=buffer, mode="w") as archive:
            for name, content in files.items():
                info = tarfile.TarInfo(name)
                info.size = len(content)
                info.mode = 0o755
                archive.addfile(info, io.BytesIO(content))
        self.entries[(key, path)] = buffer.getvalue()


SHIMS = {
    "uname": """#!/bin/sh
case "${1:-}" in -m) echo x86_64 ;; *) echo Linux ;; esac
""",
    # sudo resets the environment: only PATH survives, as with env_reset
    # and a secure_path that holds the shims.
    "sudo": """#!/bin/sh
exec env -i PATH="$PATH" "$@"
""",
    "cargo": """#!/bin/sh
sub="$1"
for dir in "${CARGO_HOME:-$HOME/.cargo}/bin" $(echo "$PATH" | tr ':' ' '); do
  if [ -x "$dir/cargo-$sub" ]; then exec "$dir/cargo-$sub" "$@"; fi
done
echo "error: no such command: $sub" >&2
exit 101
""",
    "curl": """#!/bin/sh
echo "curl $*" >> "@STATE@/calls.log"
[ -e "@STATE@/offline" ] && { echo "curl: (6) Could not resolve host" >&2; exit 6; }
out=""; url=""
while [ $# -gt 0 ]; do
  case "$1" in
    --output) out="$2"; shift 2 ;;
    https://*) url="$1"; shift ;;
    *) shift ;;
  esac
done
src="@STATE@/downloads/$(basename "$url")"
[ -f "$src" ] || { echo "curl: (22) 404 $url" >&2; exit 22; }
cp "$src" "$out"
""",
    "apt-get": """#!/bin/sh
echo "apt-get $*" >> "@STATE@/calls.log"
[ -e "@STATE@/offline" ] && { echo "E: Could not resolve archive.ubuntu.com" >&2; exit 100; }
if [ -e "@STATE@/apt-flaky" ]; then rm -f "@STATE@/apt-flaky"; echo "E: Temporary failure resolving" >&2; exit 100; fi
archives=""; cmd=""; download_only=""; pkgs=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) case "$2" in Dir::Cache::archives=*) archives="${2#Dir::Cache::archives=}" ;; esac; shift 2 ;;
    --download-only) download_only=1; shift ;;
    -y) shift ;;
    update|install|download) cmd="$1"; shift ;;
    *) pkgs="$pkgs $1"; shift ;;
  esac
done
case "$cmd" in
  update) exit 0 ;;
  install)
    [ -n "$download_only" ] || { echo "apt-get shim: only --download-only installs are expected" >&2; exit 100; }
    [ -n "$archives" ] || { echo "apt-get shim: no Dir::Cache::archives" >&2; exit 100; }
    mkdir -p "$archives/partial"
    for deb in $(cat "@STATE@/apt-set"); do echo "deb $deb" > "$archives/$deb"; done
    : > "$archives/lock"; chmod 000 "$archives/lock"
    ;;
  download)
    for p in $pkgs; do echo "deb $p" > "./${p}_1_amd64.deb"; done ;;
  *) exit 100 ;;
esac
""",
    "dpkg": """#!/bin/sh
echo "dpkg $*" >> "@STATE@/calls.log"
[ "$1" = -i ] || exit 2
shift
for deb in "$@"; do
  [ -f "$deb" ] || { echo "dpkg: error: cannot access archive '$deb'" >&2; exit 2; }
  echo "$(basename "$deb")" >> "@STATE@/dpkg-installed"
done
""",
    "systemctl": """#!/bin/sh
echo "systemctl $*" >> "@STATE@/calls.log"
""",
    "node": """#!/bin/sh
cat "@STATE@/node-version"
""",
    "npm": """#!/bin/sh
echo "npm $*" >> "@STATE@/calls.log"
[ -e "@STATE@/offline" ] && { echo "npm ERR! network ENOTFOUND" >&2; exit 1; }
if [ -e "@STATE@/npm-flaky" ]; then rm -f "@STATE@/npm-flaky"; echo "npm ERR! network ECONNRESET" >&2; exit 1; fi
prefix=""; scripts=1
while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) prefix="$2"; shift 2 ;;
    --ignore-scripts|--ignore-scripts=true) scripts=0; shift ;;
    --ignore-scripts=false) scripts=1; shift ;;
    *) shift ;;
  esac
done
spec="@STATE@/npm-tree.json"
bin_name="$(jq -r .bin "$spec")"
mods="$prefix/node_modules"
rm -rf "$mods"
mkdir -p "$mods/.bin" "$mods/pkg/bin"
jq -j .stub "$spec" > "$mods/pkg/bin/$bin_name"
chmod +x "$mods/pkg/bin/$bin_name"
ln -s "../pkg/bin/$bin_name" "$mods/.bin/$bin_name"
if [ "$(jq -r .native "$spec")" = true ] && [ "$scripts" = 1 ]; then
  # The postinstall links the native binary over the JS launcher.
  jq -j .native_body "$spec" > "$mods/pkg/native"
  chmod +x "$mods/pkg/native"
  ln -sf "../pkg/native" "$mods/.bin/$bin_name"
fi
echo '{}' > "$mods/.package-lock.json"
""",
}


class Harness:
    """One job on one runner: a workspace, a runner temp, a tool cache and
    a HOME. `cache` is shared across runners to model a later job."""

    def __init__(self, base, cache, job_env=None, name="runner"):
        # Every runner of a scenario uses the same paths, as hosted runners
        # do: the cache version derives from the path string.
        self.base = Path(base) / name
        if self.base.exists():
            shutil.rmtree(self.base)
        self.cache = cache
        self.workspace = self.base / "workspace"
        self.temp = self.base / "temp"
        self.tool_cache = self.base / "toolcache"
        self.home = self.base / "home"
        self.state = self.base / "state"
        self.shims = self.base / "shims"
        for path in (self.workspace, self.temp, self.tool_cache, self.home / ".cargo" / "bin", self.state / "downloads", self.shims):
            path.mkdir(parents=True, exist_ok=True)
        github = self.workspace / ".github"
        shutil.copytree(ROOT / ".github" / "actions", github / "actions")
        shutil.copytree(ROOT / ".github" / "scripts", github / "scripts")
        for name, body in SHIMS.items():
            shim = self.shims / name
            shim.write_text(body.replace("@STATE@", str(self.state)))
            shim.chmod(0o755)
        (self.state / "node-version").write_text("v20.19.1\n")
        self.job_env = dict(job_env or {})
        self.extra_path = []
        self.summary = self.base / "summary.md"
        self.github_env = self.base / "github_env"
        self.log = []
        self.tools = []

    # -- state helpers

    def offline(self, value=True):
        flag = self.state / "offline"
        if value:
            flag.write_text("")
        elif flag.exists():
            flag.unlink()

    def calls(self, prefix=""):
        log = self.state / "calls.log"
        lines = log.read_text().splitlines() if log.exists() else []
        return [line for line in lines if line.startswith(prefix)]

    def output(self):
        return "\n".join(self.log)

    # -- execution

    def process_env(self, step_env):
        path = os.pathsep.join([*reversed(self.extra_path), str(self.shims), os.environ["PATH"]])
        env = {
            "PATH": path,
            "HOME": str(self.home),
            "RUNNER_TEMP": str(self.temp),
            "RUNNER_TOOL_CACHE": str(self.tool_cache),
            "RUNNER_OS": "Linux",
            "RUNNER_ARCH": "X64",
            "GITHUB_WORKSPACE": str(self.workspace),
            "GITHUB_STEP_SUMMARY": str(self.summary),
            "GITHUB_ENV": str(self.github_env),
            "LANG": "C.UTF-8",
        }
        env.update(self.job_env)
        env.update(step_env)
        return env

    def context(self, inputs, steps):
        return {
            "inputs": inputs,
            "steps": steps,
            "env": dict(self.job_env),
            "runner": {"os": "Linux", "arch": "X64", "temp": str(self.temp), "tool_cache": str(self.tool_cache)},
            "github": {"workspace": str(self.workspace)},
        }

    def run_script(self, name, script, step_env, shell):
        script_file = self.base / "step.sh"
        script_file.write_text(script)
        output_file = self.base / "github_output"
        path_file = self.base / "github_path"
        output_file.write_text("")
        path_file.write_text("")
        env = self.process_env(step_env)
        env["GITHUB_OUTPUT"] = str(output_file)
        env["GITHUB_PATH"] = str(path_file)
        command = ["bash", "--noprofile", "--norc", "-eo", "pipefail", str(script_file)]
        if shell is None:
            command = ["bash", "--noprofile", "--norc", "-e", str(script_file)]
        result = subprocess.run(
            command, cwd=self.workspace, env=env, capture_output=True, text=True, timeout=STEP_TIMEOUT_SECONDS
        )
        self.log.append(f"## {name}\n{result.stdout}{result.stderr}")
        if result.returncode != 0:
            raise StepFailed(name, self.output())
        self.extra_path.extend(line for line in path_file.read_text().splitlines() if line)
        outputs = {}
        lines = output_file.read_text().split("\n")
        index = 0
        while index < len(lines):
            line = lines[index]
            heredoc = re.fullmatch(r"([^=<]+)<<(\S+)", line)
            if heredoc:
                end = lines.index(heredoc.group(2), index + 1)
                outputs[heredoc.group(1)] = "\n".join(lines[index + 1:end])
                index = end + 1
                continue
            if "=" in line:
                key, value = line.split("=", 1)
                outputs[key] = value
            index += 1
        return outputs

    def run_steps(self, steps, inputs, until=None):
        context_steps = {}
        for step in steps:
            name = step.get("name") or step.get("uses") or "run"
            context = self.context(inputs, context_steps)
            record = {"outputs": {}, "outcome": "skipped", "conclusion": "skipped"}
            if condition(step.get("if"), context):
                step_env = {key: render(value, context) for key, value in (step.get("env") or {}).items()}
                if "run" in step:
                    outputs = self.run_script(name, render(step["run"], context), step_env, step.get("shell"))
                else:
                    with_inputs = {key: render(value, context) for key, value in (step.get("with") or {}).items()}
                    self.log.append(f"## {name} ({step['uses']})")
                    outputs = self.uses(step["uses"], with_inputs)
                record = {"outputs": outputs, "outcome": "success", "conclusion": "success"}
            if step.get("id"):
                context_steps[step["id"]] = record
            if until is not None and name == until:
                break
        return context_steps

    def run_action(self, action, with_inputs):
        """Run a local composite action (a path under the workspace)."""
        directory = self.workspace / action
        definition = parse_yaml((directory / "action.yml").read_text(), str(directory))
        inputs = {}
        for key, spec in (definition.get("inputs") or {}).items():
            inputs[key] = (spec or {}).get("default") or ""
        for key, value in with_inputs.items():
            if key not in inputs:
                raise AssertionError(f"{action}: unexpected input {key!r}")
            inputs[key] = value
        steps = self.run_steps(definition["runs"]["steps"], inputs)
        context = self.context(inputs, steps)
        return {key: render(spec["value"], context) for key, spec in (definition.get("outputs") or {}).items()}

    def uses(self, uses, with_inputs):
        if uses.startswith("./"):
            return self.run_action(uses[2:], with_inputs)
        action = uses.split("@", 1)[0]
        if action == "actions/cache/restore":
            return self.cache.restore(with_inputs["key"], with_inputs["path"])
        if action == "actions/cache/save":
            return self.cache.save(with_inputs["key"], with_inputs["path"])
        if action == "taiki-e/install-action":
            return self.install_action(with_inputs["tool"])
        if action == "astral-sh/setup-uv":
            return self.setup_uv(with_inputs["version"])
        if action in ("actions/setup-node", "actions/checkout"):
            return {}
        raise AssertionError(f"the harness has no fake for {uses}")

    def install_action(self, tool):
        self.tools.append(tool)
        if (self.state / "offline").exists():
            raise StepFailed(f"install {tool}", "taiki-e/install-action: network unreachable")
        match = re.fullmatch(r"cargo-nextest@(\d+\.\d+\.\d+)", tool)
        if not match:
            raise AssertionError(f"unexpected tool {tool!r}")
        write_nextest(self.home / ".cargo" / "bin" / "cargo-nextest", match.group(1))
        return {}

    def setup_uv(self, version):
        if not version:
            raise StepFailed("setup-uv", "astral-sh/setup-uv: no version, would resolve the manifest")
        directory = self.tool_cache / "uv" / version / "x86_64"
        if not (directory / "uv").exists() or not Path(f"{directory}.complete").exists():
            raise StepFailed("setup-uv", f"astral-sh/setup-uv: {version} not in the tool cache, would download")
        self.extra_path.append(str(directory))
        return {"uv-path": str(directory / "uv"), "uv-version": version}


def write_nextest(path, version):
    """A cargo-nextest that prints the real one-line and five-line forms."""
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        "#!/bin/sh\n"
        '[ "$1" = nextest ] && shift\n'
        'case "$1" in\n'
        f'  -V) echo "cargo-nextest {version} (00af4550e 2026-09-16)" ;;\n'
        f"  --version) printf 'cargo-nextest {version} (00af4550e 2026-09-16)\\nrelease: {version}\\n"
        "commit-hash: 00af4550ec3b3b9f0e574b897b06acb95d325ba2\\ncommit-date: 2026-09-16\\n"
        "host: x86_64-unknown-linux-gnu\\n' ;;\n"
        "  *) exit 2 ;;\n"
        "esac\n"
    )
    path.chmod(0o755)


def npm_tree_spec(state, bin_name, version_line, native):
    """Describe the npm tree the npm shim installs for one runtime."""
    stub = f"#!/bin/sh\necho '{version_line}'\n"
    native_body = f"#!/bin/sh\n# native {bin_name}\necho '{version_line}'\n"
    Path(state, "npm-tree.json").write_text(
        json.dumps({"bin": bin_name, "stub": stub, "native": native, "native_body": native_body})
    )
    return native_body
