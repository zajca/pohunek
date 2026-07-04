use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use pulldown_cmark::{html, Options, Parser};
use serde_json::Value;

use crate::{collect_files, create_dir_all, read_file, remove_dir_all, XtaskError};
use crate::{SiteOptions, SiteSummary};

pub(crate) fn build_site(options: SiteOptions) -> Result<SiteSummary, XtaskError> {
    let SiteOptions {
        bundle_dir,
        manifest_path,
        output_root,
    } = options;

    let manifest_bytes = read_file(manifest_path)?;
    let manifest_value: Value =
        serde_json::from_slice(&manifest_bytes).map_err(XtaskError::Json)?;
    let pohunek_version = manifest_value
        .get("pohunek_version")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let site_dir = output_root.join("site");
    let offline_dir = output_root.join("offline");
    for dir in [&site_dir, &offline_dir] {
        if dir.exists() {
            remove_dir_all(dir)?;
        }
        create_dir_all(dir)?;
    }

    let all_files = collect_files(&bundle_dir)?;
    let md_files: Vec<_> = all_files
        .iter()
        .filter(|f| f.source_path.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect();

    let mut page_links: Vec<(String, String)> = Vec::new();
    for file in &md_files {
        let content = fs::read_to_string(&file.source_path).map_err(|source| XtaskError::Io {
            path: file.source_path.clone(),
            source,
        })?;

        let page_title = content
            .lines()
            .find_map(|line| {
                let stripped = line.trim_start_matches('#');
                (stripped.len() < line.len() && line.trim_start_matches('#').starts_with(' '))
                    .then(|| stripped.trim().to_string())
            })
            .unwrap_or_else(|| file.relative_path.to_string_lossy().into_owned());

        let parser = Parser::new_ext(&content, Options::all());
        let mut body_html = String::new();
        html::push_html(&mut body_html, parser);
        let body_html = replace_md_links_in_html(&body_html);

        let html_relative_path = file.relative_path.with_extension("html");
        let html_relative_str = html_relative_path
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let nav_href = relative_index_href(&html_relative_path);

        let page_html = render_html_page(&page_title, &pohunek_version, &nav_href, &body_html);
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

fn relative_index_href(page_path: &Path) -> String {
    let parent_depth = page_path
        .parent()
        .map_or(0, |parent| parent.components().count());
    if parent_depth == 0 {
        "index.html".to_string()
    } else {
        format!("{}index.html", "../".repeat(parent_depth))
    }
}

/// Replace `.md"` with `.html"` inside `href="…"` attributes.
fn replace_md_links_in_html(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut remaining = html;
    while let Some(start) = remaining.find("href=\"") {
        result.push_str(&remaining[..start + 6]);
        remaining = &remaining[start + 6..];
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
            result.push_str(remaining);
            remaining = "";
        }
    }
    result.push_str(remaining);
    result
}

fn render_html_page(title: &str, version: &str, nav_href: &str, body: &str) -> String {
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
<nav><a href="{nav_href}">pohunek docs</a> — v{version}</nav>
{body}
<footer><hr><small>pohunek {version} — generated from knowledge bundle</small></footer>
</body>
</html>
"#,
    )
}

fn render_index_html(version: &str, pages: &[(String, String)]) -> String {
    let mut list_items = String::new();
    for (path, title) in pages {
        let _ = writeln!(list_items, "<li><a href=\"{path}\">{title}</a></li>");
    }
    let body = format!("<h1>pohunek docs</h1>\n<ul>\n{list_items}</ul>\n");
    render_html_page("pohunek docs", version, "index.html", &body)
}
