//! CLI command reference generator.
//!
//! Produces one markdown concept file per command in the static command tree.
//! Files land in `<output_dir>/reference/cli/`.

use std::path::Path;

use crate::generators::common::{frontmatter, write_concept_file, ConceptFrontmatter};
use crate::XtaskError;

struct CommandDescriptor {
    /// Identifier used for the file name and concept `id`, e.g. `session-new`.
    id: &'static str,
    /// Full invocation title, e.g. `pohunek session new`.
    title: &'static str,
    /// Brief one-liner description.
    description: &'static str,
    /// Detailed usage line shown in the markdown body. Empty string for parent
    /// commands that only list subcommands.
    usage: &'static str,
    /// Markdown block for arguments/options, or empty for parent commands.
    arguments: &'static str,
    /// Intent tags: `help`, `debug`, `setup`, etc.
    intents: &'static [&'static str],
    /// Extra body appended below the arguments section. Used for parent-command
    /// subcommand lists.
    extra_body: &'static str,
}

/// All commands in the pohunek CLI, sorted alphabetically by id.
static COMMANDS: &[CommandDescriptor] = &[
    CommandDescriptor {
        id: "assistant",
        title: "pohunek assistant",
        description: "Launch the universal assistant.",
        usage: "pohunek assistant [options]",
        arguments: "",
        intents: &["help"],
        extra_body: "## Subcommands\n\n\
                     - `pohunek assistant setup` — Steer toward host setup.\n\
                     - `pohunek assistant project` — Steer toward project configuration.\n\
                     - `pohunek assistant update` — Steer toward reconciling an update.\n\
                     - `pohunek assistant debug` — Steer toward debugging a failure.\n\
                     - `pohunek assistant help` — Steer toward general help.",
    },
    CommandDescriptor {
        id: "assistant-debug",
        title: "pohunek assistant debug",
        description: "Steer the assistant toward debugging a failure.",
        usage: "pohunek assistant debug [options]",
        arguments: "",
        intents: &["debug"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "assistant-help",
        title: "pohunek assistant help",
        description: "Steer the assistant toward general help.",
        usage: "pohunek assistant help [options]",
        arguments: "",
        intents: &["help"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "assistant-project",
        title: "pohunek assistant project",
        description: "Steer the assistant toward project configuration.",
        usage: "pohunek assistant project [options]",
        arguments: "",
        intents: &["help", "project"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "assistant-setup",
        title: "pohunek assistant setup",
        description: "Steer the assistant toward host setup.",
        usage: "pohunek assistant setup [options]",
        arguments: "",
        intents: &["setup", "help"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "assistant-update",
        title: "pohunek assistant update",
        description: "Steer the assistant toward reconciling an update.",
        usage: "pohunek assistant update [options]",
        arguments: "",
        intents: &["help", "update"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "attach",
        title: "pohunek attach",
        description: "Attach this terminal to a local session. Press Ctrl-] to detach.",
        usage: "pohunek attach <target>",
        arguments: "- `<target>`: Session id or label to attach to.",
        intents: &["help"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "daemon",
        title: "pohunek daemon",
        description: "Manage the host daemon.",
        usage: "pohunek daemon <subcommand>",
        arguments: "",
        intents: &["help", "debug"],
        extra_body: "## Subcommands\n\n\
                     - `pohunek daemon start` — Start the host daemon (foreground by default).",
    },
    CommandDescriptor {
        id: "daemon-start",
        title: "pohunek daemon start",
        description: "Start the host daemon (foreground by default).",
        usage: "pohunek daemon start [--detach]",
        arguments: "- `--detach`: Run the daemon in the background.",
        intents: &["setup", "help"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "doctor",
        title: "pohunek doctor",
        description: "Check environment health (binaries, socket/state dir writability).",
        usage: "pohunek doctor [--json]",
        arguments: "- `--json`: Emit machine-readable JSON instead of human text.",
        intents: &["debug", "help"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "health",
        title: "pohunek health",
        description: "Query daemon health over the control socket.",
        usage: "pohunek health [--json]",
        arguments: "- `--json`: Emit machine-readable JSON instead of human text.",
        intents: &["debug", "help"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "host",
        title: "pohunek host",
        description: "Enumerate and inspect remote NetBird peers.",
        usage: "pohunek host <subcommand>",
        arguments: "",
        intents: &["help", "debug"],
        extra_body: "## Subcommands\n\n\
                     - `pohunek host discover` — Enumerate NetBird peers and probe their daemons.\n\
                     - `pohunek host list` — List known hosts with their classification.\n\
                     - `pohunek host inspect` — Inspect one host's live capabilities.",
    },
    CommandDescriptor {
        id: "host-discover",
        title: "pohunek host discover",
        description: "Enumerate NetBird peers and probe their daemons.",
        usage: "pohunek host discover [--json]",
        arguments: "- `--json`: Emit machine-readable JSON instead of human text.",
        intents: &["debug", "help"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "host-inspect",
        title: "pohunek host inspect",
        description: "Inspect one host's live capabilities.",
        usage: "pohunek host inspect <host> [--json]",
        arguments: "- `<host>`: Host name or address to inspect.\n\
                    - `--json`: Emit machine-readable JSON instead of human text.",
        intents: &["debug", "help"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "host-list",
        title: "pohunek host list",
        description: "List known hosts with their classification.",
        usage: "pohunek host list [--json]",
        arguments: "- `--json`: Emit machine-readable JSON instead of human text.",
        intents: &["help"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "integration",
        title: "pohunek integration",
        description: "Manage agent integrations.",
        usage: "pohunek integration <subcommand>",
        arguments: "",
        intents: &["setup", "help"],
        extra_body: "## Subcommands\n\n\
                     - `pohunek integration install` — Install the SessionStart hook.",
    },
    CommandDescriptor {
        id: "integration-install",
        title: "pohunek integration install",
        description: "Install the SessionStart hook that captures the native session id.",
        usage: "pohunek integration install [--agent] [--json]",
        arguments: "- `--agent <name>`: Agent type to install the hook for.\n\
                    - `--json`: Emit machine-readable JSON instead of human text.",
        intents: &["setup", "help"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "project",
        title: "pohunek project",
        description: "List, add, show, rename, and forget projects on a host.",
        usage: "pohunek project <subcommand>",
        arguments: "",
        intents: &["help", "project"],
        extra_body: "## Subcommands\n\n\
                     - `pohunek project list` — List all registered projects.\n\
                     - `pohunek project add` — Register a project by path.\n\
                     - `pohunek project show` — Show a project and its live worktrees.\n\
                     - `pohunek project rename` — Set a project's custom display name.\n\
                     - `pohunek project rm` — Forget a project record.\n\
                     - `pohunek project prompt` — Resolve one prompt by name.\n\
                     - `pohunek project action` — Resolve one action to its recipe.\n\
                     - `pohunek project actions` — List actions resolvable for a project.",
    },
    CommandDescriptor {
        id: "project-action",
        title: "pohunek project action",
        description: "Resolve one action to its recipe.",
        usage: "pohunek project action <reference> <name> [--json]",
        arguments: "- `<reference>`: Project id or label.\n\
                    - `<name>`: Action name to resolve.\n\
                    - `--json`: Emit machine-readable JSON instead of human text.",
        intents: &["help", "project"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "project-actions",
        title: "pohunek project actions",
        description: "List actions resolvable for a project.",
        usage: "pohunek project actions <reference> [--json]",
        arguments: "- `<reference>`: Project id or label.\n\
                    - `--json`: Emit machine-readable JSON instead of human text.",
        intents: &["help", "project"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "project-add",
        title: "pohunek project add",
        description: "Register a project by path.",
        usage: "pohunek project add [<path>] [--name] [--base-branch] [--json]",
        arguments: "- `<path>`: Path to the project directory (default: current directory).\n\
                    - `--name <label>`: Custom display name for the project.\n\
                    - `--base-branch <branch>`: Default base branch for worktrees.\n\
                    - `--json`: Emit machine-readable JSON instead of human text.",
        intents: &["setup", "project"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "project-list",
        title: "pohunek project list",
        description: "List known projects on the host.",
        usage: "pohunek project list [--filter] [--json]",
        arguments: "- `--filter <text>`: Filter by project name or path.\n\
                    - `--json`: Emit machine-readable JSON instead of human text.",
        intents: &["help", "project"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "project-prompt",
        title: "pohunek project prompt",
        description: "Resolve one prompt by name.",
        usage: "pohunek project prompt <reference> <name> [--json]",
        arguments: "- `<reference>`: Project id or label.\n\
                    - `<name>`: Prompt name to resolve.\n\
                    - `--json`: Emit machine-readable JSON instead of human text.",
        intents: &["help", "project"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "project-rename",
        title: "pohunek project rename",
        description: "Set a project's custom display name.",
        usage: "pohunek project rename <reference> <name> [--json]",
        arguments: "- `<reference>`: Project id or label.\n\
                    - `<name>`: New display name.\n\
                    - `--json`: Emit machine-readable JSON instead of human text.",
        intents: &["help", "project"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "project-rm",
        title: "pohunek project rm",
        description: "Forget a project record.",
        usage: "pohunek project rm <reference> [--prune-worktrees] [--json]",
        arguments: "- `<reference>`: Project id or label.\n\
                    - `--prune-worktrees`: Also remove owned git worktrees.\n\
                    - `--json`: Emit machine-readable JSON instead of human text.",
        intents: &["help", "project"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "project-show",
        title: "pohunek project show",
        description: "Show a project and its live worktrees.",
        usage: "pohunek project show <reference> [--json]",
        arguments: "- `<reference>`: Project id or label.\n\
                    - `--json`: Emit machine-readable JSON instead of human text.",
        intents: &["help", "project"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "session",
        title: "pohunek session",
        description: "Manage local PTY-backed sessions.",
        usage: "pohunek session <subcommand>",
        arguments: "",
        intents: &["help"],
        extra_body: "## Subcommands\n\n\
                     - `pohunek session new` — Start a new session.\n\
                     - `pohunek session list` — List known sessions.\n\
                     - `pohunek session inspect` — Inspect one session.\n\
                     - `pohunek session stop` — Stop one session.\n\
                     - `pohunek session input` — Send text to one session.",
    },
    CommandDescriptor {
        id: "session-input",
        title: "pohunek session input",
        description: "Send text to one session.",
        usage: "pohunek session input <target> <text> [--json]",
        arguments: "- `<target>`: Session id or label.\n\
                    - `<text>`: Text to inject into the session PTY.\n\
                    - `--json`: Emit machine-readable JSON instead of human text.",
        intents: &["help"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "session-inspect",
        title: "pohunek session inspect",
        description: "Inspect one session.",
        usage: "pohunek session inspect <target> [--json]",
        arguments: "- `<target>`: Session id or label.\n\
                    - `--json`: Emit machine-readable JSON instead of human text.",
        intents: &["debug", "help"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "session-list",
        title: "pohunek session list",
        description: "List known sessions.",
        usage: "pohunek session list [--json] [-q] [--filter]",
        arguments: "- `--json`: Emit machine-readable JSON instead of human text.\n\
                    - `-q`: Quiet output (ids only).\n\
                    - `--filter <text>`: Filter by session label or state.",
        intents: &["help"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "session-new",
        title: "pohunek session new",
        description: "Start a new session.",
        usage: "pohunek session new [options]",
        arguments: "- `--agent <name>`: Agent name to start (default: shell).\n\
                    - `--cwd <path>`: Working directory for the session.\n\
                    - `--cols <n>`: Initial terminal width in columns (default: 80).\n\
                    - `--rows <n>`: Initial terminal height in rows (default: 24).\n\
                    - `--project <ref>`: Project to run in, by id or label.\n\
                    - `--repo <path>`: Git repository to bind a dedicated worktree for.\n\
                    - `--branch <name>`: Branch to check out in a dedicated bound worktree.\n\
                    - `--base-branch <name>`: Base branch the worktree's branch is created from.\n\
                    - `--input <text>`: Initial text to inject into the session after the PTY is spawned.\n\
                    - `--yes`: Skip confirmation prompt when starting a session on a remote host.\n\
                    - `--json`: Emit machine-readable JSON instead of human text.",
        intents: &["help"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "session-stop",
        title: "pohunek session stop",
        description: "Stop one session.",
        usage: "pohunek session stop <target> [--json]",
        arguments: "- `<target>`: Session id or label.\n\
                    - `--json`: Emit machine-readable JSON instead of human text.",
        intents: &["help"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "setup",
        title: "pohunek setup",
        description: "Set up the sway/rofi launcher integration on this machine.",
        usage: "pohunek setup [--json]",
        arguments: "- `--json`: Emit machine-readable JSON instead of human text.",
        intents: &["setup", "help"],
        extra_body: "## Subcommands\n\n\
                     - `pohunek setup scripts` — Materialize the launcher scripts into the data dir's bin/.\n\
                     - `pohunek setup config` — Write a default launcher.conf and prompt templates.\n\
                     - `pohunek setup sway` — Write the sway drop-in.",
    },
    CommandDescriptor {
        id: "setup-config",
        title: "pohunek setup config",
        description: "Write a default launcher.conf and prompt templates.",
        usage: "pohunek setup config [--force] [--json]",
        arguments: "- `--force`: Overwrite existing configuration files.\n\
                    - `--json`: Emit machine-readable JSON instead of human text.",
        intents: &["setup", "help"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "setup-scripts",
        title: "pohunek setup scripts",
        description: "Materialize the launcher scripts into the data dir's bin/.",
        usage: "pohunek setup scripts [--json]",
        arguments: "- `--json`: Emit machine-readable JSON instead of human text.",
        intents: &["setup", "help"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "setup-sway",
        title: "pohunek setup sway",
        description: "Write the sway drop-in configuration fragment.",
        usage: "pohunek setup sway [--print] [--keybind] [--issue-keybind] [--json]",
        arguments: "- `--print`: Print the generated config instead of writing it.\n\
                    - `--keybind <binding>`: Override the default session-switcher keybind.\n\
                    - `--issue-keybind <binding>`: Override the default issue-picker keybind.\n\
                    - `--json`: Emit machine-readable JSON instead of human text.",
        intents: &["setup", "help"],
        extra_body: "",
    },
    CommandDescriptor {
        id: "status",
        title: "pohunek status",
        description: "Alias for `health`: show daemon status.",
        usage: "pohunek status [--json]",
        arguments: "- `--json`: Emit machine-readable JSON instead of human text.",
        intents: &["debug", "help"],
        extra_body: "",
    },
];

fn render_command(cmd: &CommandDescriptor, since: &str) -> String {
    let usage_section = if cmd.usage.is_empty() {
        String::new()
    } else {
        format!("\n## Usage\n\n```\n{}\n```\n", cmd.usage)
    };

    let arguments_section = if cmd.arguments.is_empty() {
        String::new()
    } else {
        format!("\n## Arguments\n\n{}\n", cmd.arguments)
    };

    let extra = if cmd.extra_body.is_empty() {
        String::new()
    } else {
        format!("\n{}\n", cmd.extra_body)
    };

    let yaml = frontmatter(&ConceptFrontmatter {
        concept_type: "CliCommand",
        id: &format!("cli/{}", cmd.id),
        title: cmd.title,
        description: cmd.description,
        generated_from: "static CLI descriptor",
        since: Some(since),
        tags: &["cli", "reference"],
        intents: cmd.intents,
    });

    format!(
        "{yaml}\n\
         # {title}\n\
         \n\
         {description}\n\
         {usage_section}{arguments_section}{extra}",
        yaml = yaml,
        title = cmd.title,
        description = cmd.description,
        usage_section = usage_section,
        arguments_section = arguments_section,
        extra = extra
    )
}

/// Generate CLI command reference files into `<output_dir>/reference/cli/`.
///
/// Returns the number of files written.
pub(crate) fn generate(output_dir: &Path, since: &str) -> Result<usize, XtaskError> {
    let cli_dir = output_dir.join("reference").join("cli");
    crate::create_dir_all(&cli_dir)?;

    let mut count = 0;
    for cmd in COMMANDS {
        let file_name = format!("{}.md", cmd.id);
        let dest = cli_dir.join(&file_name);
        let content = render_command(cmd, since);
        write_concept_file(&dest, &content)?;
        count += 1;
    }

    Ok(count)
}
