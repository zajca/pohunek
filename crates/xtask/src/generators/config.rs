//! Config file reference generator.
//!
//! Produces one markdown concept file per configuration file descriptor.
//! Files land in `<output_dir>/reference/config/`.

use std::path::Path;

use crate::XtaskError;

struct ConfigDescriptor {
    /// File-system slug for the concept id and output file name, e.g. `launcher-conf`.
    id: &'static str,
    /// Human-readable config file name, e.g. `launcher.conf`.
    file_name: &'static str,
    /// Brief one-liner title suffix, e.g. `Main launcher configuration`.
    title_suffix: &'static str,
    /// Brief one-liner description.
    description: &'static str,
    /// Markdown paragraph describing where the file lives.
    location: &'static str,
    /// Markdown paragraph describing the file format.
    format: &'static str,
    /// Intent tags.
    intents: &'static [&'static str],
}

/// All configuration file descriptors, sorted alphabetically by id.
static CONFIGS: &[ConfigDescriptor] = &[
    ConfigDescriptor {
        id: "actions-toml",
        file_name: "actions.toml",
        title_suffix: "Per-project action definitions",
        description: "Per-project action definitions.",
        location: "Placed in the project root or in a `.pohunek/` subdirectory. \
                   In-repo definitions shadow host-level defaults.",
        format: "TOML file. Each `[[action]]` table defines a named action with a recipe \
                 and optional prompt template reference.",
        intents: &["setup", "help", "project"],
    },
    ConfigDescriptor {
        id: "agents-toml",
        file_name: "agents/*.toml",
        title_suffix: "Agent profile configuration",
        description: "Agent profile configuration files. Each profile is a named TOML file.",
        location: "Stored in the pohunek data directory under `agents/`. \
                   The file name (without extension) is the agent name.",
        format: "TOML file. Keys configure the agent binary, startup arguments, \
                 environment variables, and PTY dimensions.",
        intents: &["setup", "help"],
    },
    ConfigDescriptor {
        id: "launcher-conf",
        file_name: "launcher.conf",
        title_suffix: "Main launcher configuration",
        description: "Main launcher configuration file. Controls host, terminal, rofi integration.",
        location: "Written to the pohunek data directory by `pohunek setup config`.",
        format: "Key-value text file. Lines beginning with `#` are comments.",
        intents: &["setup", "help"],
    },
    ConfigDescriptor {
        id: "templates-toml",
        file_name: "templates.toml",
        title_suffix: "Per-project prompt templates",
        description: "Per-project prompt templates.",
        location: "Placed in the project root or in a `.pohunek/` subdirectory. \
                   In-repo templates shadow host-level defaults.",
        format: "TOML file. Each `[[template]]` table defines a named prompt template \
                 used by actions and the assistant.",
        intents: &["setup", "help", "project"],
    },
];

fn write_concept_file(path: &Path, content: &str) -> Result<(), XtaskError> {
    if let Some(parent) = path.parent() {
        crate::create_dir_all(parent)?;
    }
    std::fs::write(path, content).map_err(|source| XtaskError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn render_config(cfg: &ConfigDescriptor, since: &str) -> String {
    let intents_yaml = cfg
        .intents
        .iter()
        .map(|i| format!("  - {i}"))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "---\n\
         type: ConfigReference\n\
         id: config/{id}\n\
         title: \"{file_name} — {title_suffix}\"\n\
         description: \"{description}\"\n\
         source_kind: generated\n\
         generated_from: \"static config descriptor\"\n\
         since: \"{since}\"\n\
         tags:\n\
           - config\n\
           - reference\n\
         intents:\n\
         {intents}\n\
         ---\n\
         \n\
         # {file_name}\n\
         \n\
         {description}\n\
         \n\
         ## Location\n\
         \n\
         {location}\n\
         \n\
         ## Format\n\
         \n\
         {format}\n",
        id = cfg.id,
        file_name = cfg.file_name,
        title_suffix = cfg.title_suffix,
        description = cfg.description,
        location = cfg.location,
        format = cfg.format,
        intents = intents_yaml,
    )
}

/// Generate config reference files into `<output_dir>/reference/config/`.
///
/// Returns the number of files written.
pub(crate) fn generate(output_dir: &Path, since: &str) -> Result<usize, XtaskError> {
    let config_dir = output_dir.join("reference").join("config");
    crate::create_dir_all(&config_dir)?;

    let mut count = 0;
    for cfg in CONFIGS {
        let dest = config_dir.join(format!("{}.md", cfg.id));
        let content = render_config(cfg, since);
        write_concept_file(&dest, &content)?;
        count += 1;
    }

    Ok(count)
}
