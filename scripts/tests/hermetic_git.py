"""Process-wide git isolation for the script tests.

A test module that runs `git` (directly, or through the script it tests)
imports this module before its first statement that is not an import. The import replaces the developer's Git
configuration for the whole test process: no global or system configuration
file, no personal ignore or attributes file (the XDG directory names a path no
account owns), a private empty `HOME` (so no `~` lookup of git, gpg or a
credential helper reaches the developer's home), no `-c` parameters, no variable
that selects a repository, a fixed identity and no prompt. The toolchain
directories of the real home stay reachable through `CARGO_HOME` and
`RUSTUP_HOME`. A global `commit.gpgsign`, `core.hooksPath`, `init.templateDir`,
`gpg.ssh.program` or `url.<base>.insteadOf` therefore cannot reach a fixture
repository, and child processes inherit the same environment.
"""

import atexit
import os
import shutil
import tempfile
from pathlib import Path

IDENTITY_NAME = "test"
IDENTITY_EMAIL = "test@example.invalid"
# Git reads `$XDG_CONFIG_HOME/git/{config,ignore,attributes}`; no account owns this path.
NO_XDG_CONFIG = "/nonexistent/pohunek-test-xdg-config"

# Variables that select a repository, a template, a transport command or a
# prompt helper, or that inject configuration; a test passes any of them
# explicitly when it needs one.
REPOSITORY_SELECTION = (
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_SYSTEM",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_SHALLOW_FILE",
    "GIT_GRAFT_FILE",
    "GIT_REPLACE_REF_BASE",
    "GIT_TEMPLATE_DIR",
    "GIT_SSH_COMMAND",
    "GIT_ASKPASS",
)


def private_home(environment):
    """Points `HOME` at a new empty directory, keeping the toolchain homes."""
    real = environment.get("HOME")
    if real:
        for variable, name in (("CARGO_HOME", ".cargo"), ("RUSTUP_HOME", ".rustup")):
            if variable not in environment and Path(real, name).is_dir():
                environment[variable] = str(Path(real, name))
    home = tempfile.mkdtemp(prefix="hg-home-")
    atexit.register(shutil.rmtree, home, ignore_errors=True)
    environment["HOME"] = home


def isolate(environment):
    """Makes `environment` (a mutable mapping) ignore the developer's Git files."""
    private_home(environment)
    for name in REPOSITORY_SELECTION:
        environment.pop(name, None)
    for name in [name for name in environment if name.startswith("GIT_CONFIG_KEY_") or name.startswith("GIT_CONFIG_VALUE_")]:
        del environment[name]
    environment.update(
        XDG_CONFIG_HOME=NO_XDG_CONFIG,
        GIT_CONFIG_GLOBAL=os.devnull,
        GIT_CONFIG_NOSYSTEM="1",
        GIT_CONFIG_COUNT="0",
        GIT_TERMINAL_PROMPT="0",
        GIT_AUTHOR_NAME=IDENTITY_NAME,
        GIT_AUTHOR_EMAIL=IDENTITY_EMAIL,
        GIT_COMMITTER_NAME=IDENTITY_NAME,
        GIT_COMMITTER_EMAIL=IDENTITY_EMAIL,
    )
    return environment


isolate(os.environ)
