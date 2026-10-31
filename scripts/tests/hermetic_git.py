"""Process-wide git isolation for the script tests.

A test module that runs `git` (directly, or through the script it tests)
imports this module first. The import replaces the developer's Git
configuration for the whole test process: no global or system configuration
file, no personal ignore or attributes file (the XDG directory names a path no
account owns), no `-c` parameters, no variable that selects a repository, a
fixed identity and no prompt. A global `commit.gpgsign`, `core.hooksPath`, `init.templateDir`,
`gpg.ssh.program` or `url.<base>.insteadOf` therefore cannot reach a fixture
repository, and child processes inherit the same environment.
"""

import os

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


def isolate(environment):
    """Makes `environment` (a mutable mapping) ignore the developer's Git files."""
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
