//! Protocol method and event reference generator.
//!
//! Produces one markdown concept file per method/event in the static descriptor
//! tables. Files land in `<output_dir>/reference/protocol/`.

use std::path::Path;

use crate::generators::common::{frontmatter, write_concept_file, ConceptFrontmatter};
use crate::XtaskError;

struct MethodDescriptor {
    /// Wire name, e.g. `daemon.health`.
    wire_name: &'static str,
    /// Brief one-liner description.
    description: &'static str,
}

struct EventDescriptor {
    /// Wire name, e.g. `agent_state`.
    wire_name: &'static str,
    /// Brief one-liner description.
    description: &'static str,
}

/// All protocol methods, sorted alphabetically by wire name.
static METHODS: &[MethodDescriptor] = &[
    MethodDescriptor {
        wire_name: "assistant.materialize",
        description: "Materialize the embedded assistant knowledge bundle on the agent host.",
    },
    MethodDescriptor {
        wire_name: "daemon.doctor",
        description: "Run daemon-local doctor checks.",
    },
    MethodDescriptor {
        wire_name: "daemon.health",
        description: "Liveness/version probe.",
    },
    MethodDescriptor {
        wire_name: "host.discover",
        description: "Enumerate and classify the local host's NetBird peers.",
    },
    MethodDescriptor {
        wire_name: "host.inspect",
        description: "Live host capability probe.",
    },
    MethodDescriptor {
        wire_name: "integration.install",
        description: "Install the per-agent SessionStart hook that captures the native id.",
    },
    MethodDescriptor {
        wire_name: "project.action",
        description: "Resolve one action by name to its recipe plus prompt content.",
    },
    MethodDescriptor {
        wire_name: "project.actions",
        description: "List available project actions after in-repo-over-host shadowing.",
    },
    MethodDescriptor {
        wire_name: "project.add",
        description: "Register (or re-add) a project by host-local path.",
    },
    MethodDescriptor {
        wire_name: "project.list",
        description: "List known projects on the target host.",
    },
    MethodDescriptor {
        wire_name: "project.prompt",
        description: "Resolve one prompt by name to its template content.",
    },
    MethodDescriptor {
        wire_name: "project.remove",
        description: "Forget a project record (optionally pruning owned worktrees).",
    },
    MethodDescriptor {
        wire_name: "project.rename",
        description: "Set a project's custom display name.",
    },
    MethodDescriptor {
        wire_name: "project.show",
        description: "Show a project plus its live worktrees.",
    },
    MethodDescriptor {
        wire_name: "session.attach",
        description: "Attach to a session's PTY stream.",
    },
    MethodDescriptor {
        wire_name: "session.detach",
        description: "Detach from a session's PTY stream.",
    },
    MethodDescriptor {
        wire_name: "session.input",
        description: "Send text input to a session.",
    },
    MethodDescriptor {
        wire_name: "session.inspect",
        description: "Inspect one session.",
    },
    MethodDescriptor {
        wire_name: "session.list",
        description: "List known sessions.",
    },
    MethodDescriptor {
        wire_name: "session.new",
        description: "Create a new PTY-backed agent session.",
    },
    MethodDescriptor {
        wire_name: "session.report_native_id",
        description: "Fire-and-forget native-session-id capture from the agent hook.",
    },
    MethodDescriptor {
        wire_name: "session.resize",
        description: "Resize a session's terminal.",
    },
    MethodDescriptor {
        wire_name: "session.set_metadata",
        description: "Merge a session's metadata map (null value deletes a key).",
    },
    MethodDescriptor {
        wire_name: "session.stop",
        description: "Stop one session.",
    },
    MethodDescriptor {
        wire_name: "status",
        description: "Query host status.",
    },
    MethodDescriptor {
        wire_name: "subscribe",
        description: "Stream daemon events as NDJSON.",
    },
];

/// All protocol events, sorted alphabetically by wire name.
static EVENTS: &[EventDescriptor] = &[
    EventDescriptor {
        wire_name: "agent_state",
        description: "Agent state changed.",
    },
    EventDescriptor {
        wire_name: "attach_closed",
        description: "A client detached from a session's PTY stream.",
    },
    EventDescriptor {
        wire_name: "attach_opened",
        description: "A client attached to a session's PTY stream.",
    },
    EventDescriptor {
        wire_name: "session_created",
        description: "A new session was created.",
    },
    EventDescriptor {
        wire_name: "session_stopped",
        description: "A session stopped.",
    },
    EventDescriptor {
        wire_name: "session_updated",
        description: "A session's metadata was updated.",
    },
];

/// Convert a method wire name to a file-system slug.
///
/// `daemon.health` → `daemon-health`
fn method_slug(wire_name: &str) -> String {
    wire_name.replace('.', "-")
}

/// Convert an event wire name to a file-system slug.
///
/// `agent_state` → `event-agent-state`
fn event_slug(wire_name: &str) -> String {
    format!("event-{}", wire_name.replace('_', "-"))
}

fn render_method(method: &MethodDescriptor, since: &str) -> String {
    let slug = method_slug(method.wire_name);
    let yaml = frontmatter(&ConceptFrontmatter {
        concept_type: "ProtocolMethod",
        id: &format!("protocol/{slug}"),
        title: &format!("{} — {}", method.wire_name, method.description),
        description: method.description,
        generated_from: "static protocol descriptor",
        since: Some(since),
        tags: &["protocol", "reference"],
        intents: &["debug"],
    });
    format!(
        "{yaml}\n\
         # {wire_name}\n\
         \n\
         {description}\n\
         \n\
         ## Wire name\n\
         \n\
         `{wire_name}`\n\
         \n\
         ## Transport\n\
         \n\
         Sent over the local Unix control socket as a newline-delimited JSON request envelope.\n",
        yaml = yaml,
        wire_name = method.wire_name,
        description = method.description,
    )
}

fn render_event(event: &EventDescriptor, since: &str) -> String {
    let slug = event_slug(event.wire_name);
    let yaml = frontmatter(&ConceptFrontmatter {
        concept_type: "ProtocolEvent",
        id: &format!("protocol/{slug}"),
        title: &format!("{} — {}", event.wire_name, event.description),
        description: event.description,
        generated_from: "static protocol descriptor",
        since: Some(since),
        tags: &["protocol", "reference"],
        intents: &["debug"],
    });
    format!(
        "{yaml}\n\
         # {wire_name}\n\
         \n\
         {description}\n\
         \n\
         ## Wire name\n\
         \n\
         `{wire_name}`\n\
         \n\
         ## Transport\n\
         \n\
         Published on subscription connections as newline-delimited JSON event envelopes.\n",
        yaml = yaml,
        wire_name = event.wire_name,
        description = event.description,
    )
}

/// Generate protocol method and event reference files into
/// `<output_dir>/reference/protocol/`.
///
/// Returns the number of files written.
pub(crate) fn generate(output_dir: &Path, since: &str) -> Result<usize, XtaskError> {
    let protocol_dir = output_dir.join("reference").join("protocol");
    crate::create_dir_all(&protocol_dir)?;

    let mut count = 0;

    for method in METHODS {
        let slug = method_slug(method.wire_name);
        let dest = protocol_dir.join(format!("{slug}.md"));
        let content = render_method(method, since);
        write_concept_file(&dest, &content)?;
        count += 1;
    }

    for event in EVENTS {
        let slug = event_slug(event.wire_name);
        let dest = protocol_dir.join(format!("{slug}.md"));
        let content = render_event(event, since);
        write_concept_file(&dest, &content)?;
        count += 1;
    }

    Ok(count)
}
