//! Setup asset reference generator.
//!
//! Produces one markdown concept file per embedded launcher script.
//! Files land in `<output_dir>/reference/setup-assets/`.

use std::path::Path;

use crate::generators::common::{frontmatter, write_concept_file, ConceptFrontmatter};
use crate::XtaskError;

struct AssetDescriptor {
    /// File-system slug derived from the script name, e.g. `lib-sh`.
    id: &'static str,
    /// Original script name, e.g. `lib.sh`.
    script_name: &'static str,
    /// Brief title suffix, e.g. `Shared shell library`.
    title_suffix: &'static str,
    /// Brief one-liner description.
    description: &'static str,
}

/// All setup assets, sorted alphabetically by id.
///
/// Mirrors the `SCRIPTS` constant in `crates/cli/src/commands/setup.rs`.
static ASSETS: &[AssetDescriptor] = &[
    AssetDescriptor {
        id: "lib-sh",
        script_name: "lib.sh",
        title_suffix: "Shared shell library",
        description: "Shared shell library sourced by all pohunek launcher scripts.",
    },
    AssetDescriptor {
        id: "pohunek-launch-issue",
        script_name: "pohunek-launch-issue",
        title_suffix: "Launch a session from a Linear issue",
        description: "Launch a session from a Linear issue.",
    },
    AssetDescriptor {
        id: "pohunek-launch-pr",
        script_name: "pohunek-launch-pr",
        title_suffix: "Launch a session from a GitHub pull request",
        description: "Launch a session from a GitHub pull request.",
    },
    AssetDescriptor {
        id: "pohunek-rofi",
        script_name: "pohunek-rofi",
        title_suffix: "Rofi-based session switcher launcher",
        description: "Rofi-based session switcher launcher.",
    },
    AssetDescriptor {
        id: "pohunek-rofi-issue",
        script_name: "pohunek-rofi-issue",
        title_suffix: "Rofi-based Linear issue picker",
        description: "Rofi-based Linear issue picker.",
    },
];

fn render_asset(asset: &AssetDescriptor) -> String {
    let yaml = frontmatter(&ConceptFrontmatter {
        concept_type: "SetupAsset",
        id: &format!("setup-assets/{}", asset.id),
        title: &format!("{} — {}", asset.script_name, asset.title_suffix),
        description: asset.description,
        generated_from: "static setup asset descriptor",
        since: None,
        tags: &["setup", "reference"],
        intents: &["setup", "help"],
    });
    format!(
        "{yaml}\n\
         # {script_name}\n\
         \n\
         {description}\n\
         \n\
         ## Deployment\n\
         \n\
        Materialized to the pohunek data directory's `bin/` subdirectory by \
         `pohunek setup scripts`.\n",
        yaml = yaml,
        script_name = asset.script_name,
        description = asset.description,
    )
}

/// Generate setup asset reference files into `<output_dir>/reference/setup-assets/`.
///
/// Returns the number of files written.
///
/// Note: `SetupAsset` does not require a `since` field per the schema, so no
/// `since` parameter is used here.
pub(crate) fn generate(output_dir: &Path, _since: &str) -> Result<usize, XtaskError> {
    let assets_dir = output_dir.join("reference").join("setup-assets");
    crate::create_dir_all(&assets_dir)?;

    let mut count = 0;
    for asset in ASSETS {
        let dest = assets_dir.join(format!("{}.md", asset.id));
        let content = render_asset(asset);
        write_concept_file(&dest, &content)?;
        count += 1;
    }

    Ok(count)
}
