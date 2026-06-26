#![warn(missing_debug_implementations)]
#![warn(rust_2018_idioms)]
#![warn(unreachable_pub)]
#![forbid(unsafe_code)]

mod eval;
mod generators;

use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use knowledge::{validate_bundle, CONCEPT_SCHEMA_VERSION};
use pulldown_cmark::{html, Options, Parser};
use regex::Regex;
use serde::Serialize;
use sha2::{Digest, Sha256};

const MANUAL_SOURCE: &str = "docs/knowledge";
const GENERATED_REFERENCE: &str = "generated";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationSummary {
    pub schema_version: u32,
    pub files_checked: usize,
    pub concept_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildOptions {
    pub source_dir: PathBuf,
    pub output_root: PathBuf,
    pub pohunek_version: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildSummary {
    pub bundle_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub content_hash: String,
    pub files_copied: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SiteOptions {
    pub bundle_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub output_root: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SiteSummary {
    pub site_dir: PathBuf,
    pub offline_dir: PathBuf,
    pub pages_rendered: usize,
    pub pohunek_version: String,
}

#[derive(Debug)]
pub enum XtaskError {
    Usage(String),
    BundleValidation(knowledge::BundleValidationError),
    Io { path: PathBuf, source: io::Error },
    UnsupportedFileType(PathBuf),
    Json(serde_json::Error),
    InvalidPath(PathBuf),
}

impl fmt::Display for XtaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) => write!(formatter, "{message}"),
            Self::BundleValidation(error) => write!(formatter, "{error}"),
            Self::Io { path, source } => {
                write!(formatter, "failed to access `{}`: {source}", path.display())
            }
            Self::UnsupportedFileType(path) => {
                write!(formatter, "unsupported file type in `{}`", path.display())
            }
            Self::Json(error) => write!(formatter, "failed to write manifest json: {error}"),
            Self::InvalidPath(path) => write!(
                formatter,
                "path `{}` cannot be represented as a deterministic relative path",
                path.display()
            ),
        }
    }
}

impl Error for XtaskError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::BundleValidation(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            Self::Json(error) => Some(error),
            Self::Usage(_) | Self::UnsupportedFileType(_) | Self::InvalidPath(_) => None,
        }
    }
}

#[derive(Serialize)]
struct Manifest<'a> {
    pohunek_version: &'a str,
    knowledge_schema_version: u32,
    reference: &'a str,
    sources: Vec<&'a str>,
    content_hash: &'a str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileEntry {
    source_path: PathBuf,
    relative_path: PathBuf,
}

pub fn validate_docs(source_dir: impl AsRef<Path>) -> Result<ValidationSummary, XtaskError> {
    let report = validate_bundle(source_dir).map_err(XtaskError::BundleValidation)?;
    Ok(ValidationSummary {
        schema_version: report.schema_version,
        files_checked: report.files_checked,
        concept_count: report.concepts.len(),
    })
}

pub fn build_docs(options: BuildOptions) -> Result<BuildSummary, XtaskError> {
    let bundle_dir = options.output_root.join("knowledge-bundle");
    if bundle_dir.exists() {
        remove_dir_all(&bundle_dir)?;
    }
    create_dir_all(&bundle_dir)?;

    let since = options.pohunek_version.as_str();

    // Phase 1: run all reference generators into the bundle dir.
    generators::cli::generate(&bundle_dir, since)?;
    generators::config::generate(&bundle_dir, since)?;
    generators::protocol::generate(&bundle_dir, since)?;
    generators::setup_assets::generate(&bundle_dir, since)?;

    // Phase 2: copy manual docs into the bundle dir.
    let manual_files = collect_files(&options.source_dir)?;
    for file in &manual_files {
        let destination = bundle_dir.join(&file.relative_path);
        if let Some(parent) = destination.parent() {
            create_dir_all(parent)?;
        }
        copy_file(&file.source_path, &destination)?;
    }

    // Phase 3: validate the merged bundle.
    validate_docs(&bundle_dir)?;

    // Phase 4: hash the merged bundle and write the manifest.
    let merged_files = collect_files(&bundle_dir)?;
    let content_hash = content_hash(&bundle_dir, &merged_files)?;
    let manifest_path = options.output_root.join("manifest.json");
    let manifest = Manifest {
        pohunek_version: &options.pohunek_version,
        knowledge_schema_version: CONCEPT_SCHEMA_VERSION,
        reference: GENERATED_REFERENCE,
        sources: vec!["cli", "config", "manual_docs", "protocol", "setup_assets"],
        content_hash: &content_hash,
    };
    write_manifest(&manifest_path, &manifest)?;

    Ok(BuildSummary {
        bundle_dir,
        manifest_path,
        content_hash,
        files_copied: merged_files.len(),
    })
}

/// Render the merged knowledge bundle to static HTML pages.
///
/// Reads the manifest for the `pohunek_version`, walks all `.md` files in
/// `bundle_dir` (in sorted order for determinism), converts each to HTML using
/// `pulldown-cmark`, and writes the result to both `site/` and `offline/`
/// under `output_root`. An `index.html` listing all pages is written to each
/// output directory. No external resources are used — all CSS is inlined.
pub fn build_site(options: SiteOptions) -> Result<SiteSummary, XtaskError> {
    // Read the manifest to obtain the pohunek version.
    let manifest_bytes = read_file(options.manifest_path.clone())?;
    let manifest_value: serde_json::Value =
        serde_json::from_slice(&manifest_bytes).map_err(XtaskError::Json)?;
    let pohunek_version = manifest_value
        .get("pohunek_version")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    // Set up output directories.
    let site_dir = options.output_root.join("site");
    let offline_dir = options.output_root.join("offline");
    for dir in [&site_dir, &offline_dir] {
        if dir.exists() {
            remove_dir_all(dir)?;
        }
        create_dir_all(dir)?;
    }

    // Collect all files from the bundle dir (already sorted by collect_files).
    let all_files = collect_files(&options.bundle_dir)?;
    let md_files: Vec<&FileEntry> = all_files
        .iter()
        .filter(|f| f.source_path.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect();

    // Track rendered page relative HTML paths for the index.
    let mut page_links: Vec<(String, String)> = Vec::new(); // (relative html path, page title)

    for file in &md_files {
        let content = fs::read_to_string(&file.source_path).map_err(|source| XtaskError::Io {
            path: file.source_path.clone(),
            source,
        })?;

        // Extract the first `# Heading` as the page title.
        let page_title = content
            .lines()
            .find_map(|line| {
                let stripped = line.trim_start_matches('#');
                if stripped.len() < line.len() && line.trim_start_matches('#').starts_with(' ') {
                    Some(stripped.trim().to_string())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| file.relative_path.to_string_lossy().into_owned());

        // Convert Markdown to HTML using pulldown-cmark.
        let parser = Parser::new_ext(&content, Options::all());
        let mut body_html = String::new();
        html::push_html(&mut body_html, parser);

        // Translate relative `.md` links to `.html` links.
        let body_html = body_html.replace("href=\"", "href=\"__PLACEHOLDER__");
        let body_html = body_html.replace("__PLACEHOLDER__", "");
        // Simpler direct replacement of .md" -> .html" in href attributes.
        let body_html = replace_md_links_in_html(&body_html);

        // Build the HTML page using the skeleton.
        let page_html = render_html_page(&page_title, &pohunek_version, &body_html);

        // Determine output path: change .md extension to .html.
        let html_relative_path = file.relative_path.with_extension("html");
        let html_relative_str = html_relative_path
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");

        // Write to both site/ and offline/.
        for out_root in [&site_dir, &offline_dir] {
            let out_path = out_root.join(&html_relative_path);
            if let Some(parent) = out_path.parent() {
                create_dir_all(parent)?;
            }
            fs::write(&out_path, &page_html).map_err(|source| XtaskError::Io {
                path: out_path.clone(),
                source,
            })?;
        }

        page_links.push((html_relative_str, page_title));
    }

    // Write index.html to both output dirs.
    let index_html = render_index_html(&pohunek_version, &page_links);
    for out_root in [&site_dir, &offline_dir] {
        let index_path = out_root.join("index.html");
        fs::write(&index_path, &index_html).map_err(|source| XtaskError::Io {
            path: index_path.clone(),
            source,
        })?;
    }

    Ok(SiteSummary {
        site_dir,
        offline_dir,
        pages_rendered: md_files.len(),
        pohunek_version,
    })
}

/// Replace `.md"` with `.html"` inside `href="…"` attributes so that
/// inter-page links remain functional in the rendered site.
fn replace_md_links_in_html(html: &str) -> String {
    // We look for the pattern href="...*.md" and replace only the .md extension.
    // A simple pass using find is sufficient because all hrefs in generated HTML
    // come from pulldown-cmark and are well-formed.
    let mut result = String::with_capacity(html.len());
    let mut remaining = html;
    while let Some(start) = remaining.find("href=\"") {
        result.push_str(&remaining[..start + 6]); // push up to and including href="
        remaining = &remaining[start + 6..];
        // Find the closing quote.
        if let Some(end) = remaining.find('"') {
            let href_value = &remaining[..end];
            if let Some(stem) = href_value.strip_suffix(".md") {
                result.push_str(stem);
                result.push_str(".html");
            } else {
                result.push_str(href_value);
            }
            result.push('"');
            remaining = &remaining[end + 1..];
        } else {
            // Malformed href — just append the rest as-is.
            result.push_str(remaining);
            remaining = "";
        }
    }
    result.push_str(remaining);
    result
}

/// Wrap a body HTML fragment in the full page skeleton.
fn render_html_page(title: &str, version: &str, body: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} — pohunek {version}</title>
<style>
body{{font-family:system-ui,sans-serif;max-width:860px;margin:0 auto;padding:1rem 2rem}}
pre{{background:#f5f5f5;padding:1rem;overflow-x:auto}}
code{{background:#f5f5f5;padding:0.1em 0.3em}}
a{{color:#0969da}}
nav{{border-bottom:1px solid #d0d7de;padding-bottom:0.5rem;margin-bottom:1.5rem}}
</style>
</head>
<body>
<nav><a href="/index.html">pohunek docs</a> — v{version}</nav>
{body}
<footer><hr><small>pohunek {version} — generated from knowledge bundle</small></footer>
</body>
</html>
"#,
        title = title,
        version = version,
        body = body,
    )
}

/// Render the root `index.html` that links to all generated pages.
fn render_index_html(version: &str, pages: &[(String, String)]) -> String {
    let mut list_items = String::new();
    for (path, title) in pages {
        list_items.push_str(&format!("<li><a href=\"{path}\">{title}</a></li>\n"));
    }
    let body = format!("<h1>pohunek docs</h1>\n<ul>\n{list_items}</ul>\n");
    render_html_page("pohunek docs", version, &body)
}

/// Run all drift checks for the knowledge bundle. Returns `Ok(true)` if every
/// check passes, `Ok(false)` if any check fails (the caller should exit
/// non-zero). Each check prints a `[PASS]` or `[FAIL]` line to stdout.
///
/// Checks performed:
///   1. Schema validation — the manual source directory is a valid bundle.
///   2. Deterministic build — two independent builds produce the same
///      `content_hash`.
///   3. Source-map path existence — every backtick-quoted `crates/…` or
///      `docs/…` path in `assistant/source-map.md` resolves in the repo.
///   4. Runbook-vs-parser — every `pohunek …` example command in the runbooks
///      (except placeholder lines that contain `<…>` tokens) parses cleanly
///      against the live `pohunek_cli::command()` clap tree.
///   5. Secret scan — the built knowledge bundle contains no secret-like
///      strings matching known credential patterns.
pub fn check_docs(
    source_dir: impl AsRef<Path>,
    output_root: impl AsRef<Path>,
) -> Result<bool, XtaskError> {
    let source_dir = source_dir.as_ref();
    let output_root = output_root.as_ref();
    let repo = repo_root();
    let mut all_pass = true;

    // ------------------------------------------------------------------
    // Check 1: schema validation
    // ------------------------------------------------------------------
    match validate_docs(source_dir) {
        Ok(summary) => {
            println!(
                "[PASS] schema-validation: {} files, {} concepts, schema v{}",
                summary.files_checked, summary.concept_count, summary.schema_version
            );
        }
        Err(err) => {
            println!("[FAIL] schema-validation: {err}");
            all_pass = false;
        }
    }

    // ------------------------------------------------------------------
    // Check 2: deterministic build (two independent builds, same hash)
    // ------------------------------------------------------------------
    let build1_root = output_root.join("check-build-1");
    let build2_root = output_root.join("check-build-2");

    // Clean up any previous check artifacts.
    if build1_root.exists() {
        remove_dir_all(&build1_root)?;
    }
    if build2_root.exists() {
        remove_dir_all(&build2_root)?;
    }
    create_dir_all(&build1_root)?;
    create_dir_all(&build2_root)?;

    let version = env!("CARGO_PKG_VERSION").to_string();
    let build1 = build_docs(BuildOptions {
        source_dir: source_dir.to_path_buf(),
        output_root: build1_root.clone(),
        pohunek_version: version.clone(),
    });
    let build2 = build_docs(BuildOptions {
        source_dir: source_dir.to_path_buf(),
        output_root: build2_root.clone(),
        pohunek_version: version,
    });

    match (build1, build2) {
        (Ok(s1), Ok(s2)) => {
            if s1.content_hash == s2.content_hash {
                println!(
                    "[PASS] deterministic-build: both runs produced hash {}",
                    s1.content_hash
                );
            } else {
                println!(
                    "[FAIL] deterministic-build: hashes differ: {} vs {}",
                    s1.content_hash, s2.content_hash
                );
                all_pass = false;
            }
        }
        (Err(err), _) | (_, Err(err)) => {
            println!("[FAIL] deterministic-build: build failed: {err}");
            all_pass = false;
        }
    }

    // Clean up temporary build trees.
    if build1_root.exists() {
        remove_dir_all(&build1_root)?;
    }
    if build2_root.exists() {
        remove_dir_all(&build2_root)?;
    }

    // ------------------------------------------------------------------
    // Check 3: source-map path existence
    // ------------------------------------------------------------------
    let source_map_path = source_dir.join("assistant/source-map.md");
    match fs::read_to_string(&source_map_path) {
        Ok(content) => {
            // Match every backtick-quoted token that looks like a repo path.
            // The regex captures the content inside a pair of backticks.
            let backtick_re = Regex::new(r"`([^`]+)`").expect("valid backtick regex");
            let mut missing: Vec<String> = Vec::new();

            for captures in backtick_re.captures_iter(&content) {
                let candidate = captures[1].to_string();
                // Only consider paths that look like repo-relative paths.
                if candidate.starts_with("crates/") || candidate.starts_with("docs/") {
                    let full_path = repo.join(&candidate);
                    if !full_path.exists() {
                        missing.push(candidate);
                    }
                }
            }

            if missing.is_empty() {
                println!("[PASS] source-map-paths: all referenced paths exist");
            } else {
                println!(
                    "[FAIL] source-map-paths: {} missing path(s):",
                    missing.len()
                );
                for path in &missing {
                    println!("        {path}");
                }
                all_pass = false;
            }
        }
        Err(source) => {
            println!(
                "[FAIL] source-map-paths: could not read {}: {source}",
                source_map_path.display()
            );
            all_pass = false;
        }
    }

    // ------------------------------------------------------------------
    // Check 4: runbook-vs-parser
    //
    // Extract `pohunek …` command examples from all runbooks and parse each
    // against the live clap command tree. Lines that contain `<…>` tokens
    // (placeholders) are skipped because they are not real commands.
    // ------------------------------------------------------------------
    let runbooks_dir = source_dir.join("runbooks");
    let mut runbook_failures: Vec<String> = Vec::new();
    let mut runbook_checked = 0usize;

    // Regex: backtick-quoted strings starting with `pohunek `.
    let backtick_cmd_re =
        Regex::new(r"`(pohunek [^`]+)`").expect("valid runbook backtick command regex");

    if runbooks_dir.exists() {
        let entries = collect_files(&runbooks_dir)?;
        for entry in entries {
            if entry.source_path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let content = match fs::read_to_string(&entry.source_path) {
                Ok(c) => c,
                Err(source) => {
                    runbook_failures.push(format!(
                        "{}: could not read file: {source}",
                        entry.source_path.display()
                    ));
                    continue;
                }
            };

            for line in content.lines() {
                // Collect command strings from this line.
                let mut candidates: Vec<String> = Vec::new();

                // 1. Backtick-quoted `pohunek …` substrings.
                for cap in backtick_cmd_re.captures_iter(line) {
                    candidates.push(cap[1].to_string());
                }

                // 2. Bare `pohunek …` at start of line after stripping list
                //    markers and leading whitespace.
                let trimmed = line.trim_start_matches(|c: char| {
                    c.is_ascii_whitespace()
                        || c == '-'
                        || c == '*'
                        || c == '+'
                        || c.is_ascii_digit()
                        || c == '.'
                });
                let trimmed = trimmed.trim_start();
                if trimmed.starts_with("pohunek ") && !backtick_cmd_re.is_match(line) {
                    candidates.push(trimmed.to_string());
                }

                for cmd_str in candidates {
                    // Skip placeholder lines (contain `<…>` tokens).
                    if cmd_str.contains('<') {
                        continue;
                    }
                    // Split on whitespace into tokens (basic quoting is not
                    // needed; runbook examples use simple shell words).
                    let tokens: Vec<String> =
                        cmd_str.split_whitespace().map(str::to_string).collect();

                    runbook_checked += 1;

                    // A fresh command is required each call because
                    // `try_get_matches_from` consumes the Command value.
                    match cli::command().try_get_matches_from(&tokens) {
                        Ok(_) => {}
                        Err(err) => {
                            use clap::error::ErrorKind;
                            // DisplayHelp / DisplayVersion are valid outcomes
                            // (e.g. `pohunek --help`); only real parse errors fail.
                            if err.kind() != ErrorKind::DisplayHelp
                                && err.kind() != ErrorKind::DisplayVersion
                            {
                                runbook_failures.push(format!(
                                    "{}: command `{cmd_str}` failed to parse: {}",
                                    entry.source_path.display(),
                                    err.kind().as_str().unwrap_or("unknown")
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    if runbook_failures.is_empty() {
        println!("[PASS] runbook-commands: {runbook_checked} command(s) parsed successfully");
    } else {
        println!(
            "[FAIL] runbook-commands: {} command(s) failed to parse (checked {runbook_checked}):",
            runbook_failures.len()
        );
        for failure in &runbook_failures {
            println!("        {failure}");
        }
        all_pass = false;
    }

    // ------------------------------------------------------------------
    // Check 5: secret scan of the built knowledge bundle
    //
    // Build the bundle (reusing the main output_root so it stays around for
    // normal use after `docs check`). Walk all .md files and flag any line
    // that matches one of the secret-like patterns below.
    // ------------------------------------------------------------------

    // Build the bundle into the main output root for the secret scan.
    let bundle_root = output_root.join("check-secret-scan");
    if bundle_root.exists() {
        remove_dir_all(&bundle_root)?;
    }
    create_dir_all(&bundle_root)?;

    let secret_scan_result = build_docs(BuildOptions {
        source_dir: source_dir.to_path_buf(),
        output_root: bundle_root.clone(),
        pohunek_version: env!("CARGO_PKG_VERSION").to_string(),
    });

    match secret_scan_result {
        Err(err) => {
            println!("[FAIL] secret-scan: build for scan failed: {err}");
            all_pass = false;
        }
        Ok(build_summary) => {
            // Patterns that indicate a secret-like assignment.
            // Using (?i) for case-insensitive matching.
            let patterns: &[(&str, &str)] = &[
                // API key assignments (e.g. api_key=abc123)
                (r"(?i)api[_-]?key\s*[:=]\s*\S+", "api-key assignment"),
                // Token assignments (e.g. token=abc123)
                (r"(?i)token\s*[:=]\s*\S+", "token assignment"),
                // PEM private key headers
                (
                    r"-----BEGIN [A-Z ]+PRIVATE KEY-----",
                    "PEM private key header",
                ),
                // TOML [env] section header (a separate pattern detects key=value
                // assignments on subsequent lines; flagging the section header
                // itself is sufficient to prompt manual review of adjacent lines)
                (r"(?i)^\s*\[env\]\s*$", "TOML [env] section header"),
                // Credential-named key=value with a non-trivial value (8+ chars)
                (
                    r"(?i)(secret|password|passwd|api_key|private_key|auth_token|access_token)\s*[:=]\s*\S{8,}",
                    "credential assignment",
                ),
            ];

            let compiled: Vec<(Regex, &str)> = patterns
                .iter()
                .map(|(pat, label)| (Regex::new(pat).expect("secret scan regex is valid"), *label))
                .collect();

            let bundle_dir = &build_summary.bundle_dir;
            let bundle_files = collect_files(bundle_dir)?;
            let mut secret_hits: Vec<String> = Vec::new();

            for file in &bundle_files {
                if file.source_path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                let content = match fs::read_to_string(&file.source_path) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                for (i, line) in content.lines().enumerate() {
                    for (re, label) in &compiled {
                        if re.is_match(line) {
                            secret_hits.push(format!(
                                "{}:{}: [{}] {}",
                                file.source_path.display(),
                                i + 1,
                                label,
                                line.trim()
                            ));
                        }
                    }
                }
            }

            // Clean up the secret scan bundle.
            if bundle_root.exists() {
                remove_dir_all(&bundle_root)?;
            }

            if secret_hits.is_empty() {
                println!(
                    "[PASS] secret-scan: no credential patterns found in {} bundle file(s)",
                    bundle_files.len()
                );
            } else {
                println!(
                    "[FAIL] secret-scan: {} potential secret(s) found:",
                    secret_hits.len()
                );
                for hit in &secret_hits {
                    println!("        {hit}");
                }
                all_pass = false;
            }
        }
    }

    Ok(all_pass)
}

pub fn run<I, S>(args: I) -> Result<(), XtaskError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut args = args.into_iter().map(Into::into);
    let Some(command) = args.next().and_then(|arg| arg.into_string().ok()) else {
        return Err(usage_error());
    };

    // `eval` is a standalone top-level command with no sub-action.
    if command == "eval" {
        if args.next().is_some() {
            return Err(usage_error());
        }
        let all_pass = eval::run_eval();
        if all_pass {
            return Ok(());
        } else {
            return Err(XtaskError::Usage(
                "eval: one or more fixture commands failed to parse".to_string(),
            ));
        }
    }

    // All other commands require an action sub-argument.
    let Some(action) = args.next().and_then(|arg| arg.into_string().ok()) else {
        return Err(usage_error());
    };
    if args.next().is_some() {
        return Err(usage_error());
    }

    if command != "docs" {
        return Err(usage_error());
    }

    let root = repo_root();
    match action.as_str() {
        "validate" => {
            let summary = validate_docs(root.join(MANUAL_SOURCE))?;
            println!(
                "docs validate ok: {} files, {} concepts, schema v{}",
                summary.files_checked, summary.concept_count, summary.schema_version
            );
            Ok(())
        }
        "build" => {
            let summary = build_docs(BuildOptions {
                source_dir: root.join(MANUAL_SOURCE),
                output_root: root.join("target/pohunek-docs"),
                pohunek_version: env!("CARGO_PKG_VERSION").to_string(),
            })?;
            println!(
                "docs build ok: {} files, reference generated, hash {}",
                summary.files_copied, summary.content_hash
            );
            Ok(())
        }
        "check" => {
            let all_pass = check_docs(root.join(MANUAL_SOURCE), root.join("target/pohunek-docs"))?;
            if all_pass {
                println!("docs check ok: all checks passed");
                Ok(())
            } else {
                Err(XtaskError::Usage(
                    "docs check failed: one or more checks did not pass".to_string(),
                ))
            }
        }
        "site" => {
            // Ensure the bundle is up-to-date before rendering the site.
            let build_summary = build_docs(BuildOptions {
                source_dir: root.join(MANUAL_SOURCE),
                output_root: root.join("target/pohunek-docs"),
                pohunek_version: env!("CARGO_PKG_VERSION").to_string(),
            })?;
            let site_summary = build_site(SiteOptions {
                bundle_dir: build_summary.bundle_dir,
                manifest_path: build_summary.manifest_path,
                output_root: root.join("target/pohunek-docs"),
            })?;
            println!(
                "docs site ok: {} pages, site at {}, offline at {}",
                site_summary.pages_rendered,
                site_summary.site_dir.display(),
                site_summary.offline_dir.display()
            );
            Ok(())
        }
        _ => Err(usage_error()),
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask crate lives under crates/xtask")
        .to_path_buf()
}

fn collect_files(root: &Path) -> Result<Vec<FileEntry>, XtaskError> {
    let mut files = Vec::new();
    collect_files_inner(root, root, &mut files)?;
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(files)
}

fn collect_files_inner(
    root: &Path,
    current: &Path,
    files: &mut Vec<FileEntry>,
) -> Result<(), XtaskError> {
    let mut entries = read_dir(current)?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        let file_type = file_type(&path)?;
        if file_type.is_dir() {
            collect_files_inner(root, &path, files)?;
        } else if file_type.is_file() {
            let relative_path = path
                .strip_prefix(root)
                .map_err(|_| XtaskError::InvalidPath(path.clone()))?
                .to_path_buf();
            files.push(FileEntry {
                source_path: path,
                relative_path,
            });
        } else {
            return Err(XtaskError::UnsupportedFileType(path));
        }
    }

    Ok(())
}

fn content_hash(root: &Path, files: &[FileEntry]) -> Result<String, XtaskError> {
    let mut hasher = Sha256::new();
    for file in files {
        let relative_path = relative_path_string(&file.relative_path)?;
        let bytes = read_file(root.join(&file.relative_path))?;

        hasher.update(relative_path.as_bytes());
        hasher.update([0]);
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update([0]);
        hasher.update(bytes);
        hasher.update([0xff]);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn relative_path_string(path: &Path) -> Result<String, XtaskError> {
    let mut parts = Vec::new();
    for component in path.components() {
        let std::path::Component::Normal(part) = component else {
            return Err(XtaskError::InvalidPath(path.to_path_buf()));
        };
        let Some(part) = part.to_str() else {
            return Err(XtaskError::InvalidPath(path.to_path_buf()));
        };
        parts.push(part);
    }
    Ok(parts.join("/"))
}

fn write_manifest(path: &Path, manifest: &Manifest<'_>) -> Result<(), XtaskError> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }
    let mut json = serde_json::to_string_pretty(manifest).map_err(XtaskError::Json)?;
    json.push('\n');
    fs::write(path, json).map_err(|source| XtaskError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn read_dir(path: &Path) -> Result<Vec<fs::DirEntry>, XtaskError> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path).map_err(|source| XtaskError::Io {
        path: path.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| XtaskError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        entries.push(entry);
    }
    Ok(entries)
}

fn file_type(path: &Path) -> Result<fs::FileType, XtaskError> {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type())
        .map_err(|source| XtaskError::Io {
            path: path.to_path_buf(),
            source,
        })
}

fn read_file(path: PathBuf) -> Result<Vec<u8>, XtaskError> {
    fs::read(&path).map_err(|source| XtaskError::Io { path, source })
}

fn copy_file(source: &Path, destination: &Path) -> Result<(), XtaskError> {
    fs::copy(source, destination)
        .map(|_| ())
        .map_err(|source_error| XtaskError::Io {
            path: destination.to_path_buf(),
            source: source_error,
        })
}

pub(crate) fn create_dir_all(path: &Path) -> Result<(), XtaskError> {
    fs::create_dir_all(path).map_err(|source| XtaskError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn remove_dir_all(path: &Path) -> Result<(), XtaskError> {
    fs::remove_dir_all(path).map_err(|source| XtaskError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn usage_error() -> XtaskError {
    XtaskError::Usage(
        "usage: cargo xtask eval\n       cargo xtask docs <validate|build|check|site>".to_string(),
    )
}
