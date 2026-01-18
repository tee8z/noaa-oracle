use minify_js::{minify, Session, TopLevelMode};
use sha2::{Digest, Sha256};
use std::{env, fs, path::Path};
use walkdir::WalkDir;

fn main() {
    let manifest = env::var("CARGO_MANIFEST_DIR").unwrap();
    let templates = Path::new(&manifest).join("src/templates");
    // Output to source tree (like keymeld) so it's available at runtime
    // without needing to copy from target directory
    let output = Path::new(&manifest).join("static");

    if !templates.exists() {
        return;
    }

    // Track changes for JS and CSS
    println!("cargo:rerun-if-changed={}", templates.display());
    for entry in WalkDir::new(&templates).into_iter().filter_map(|e| e.ok()) {
        let ext = entry.path().extension().and_then(|e| e.to_str());
        if matches!(ext, Some("js") | Some("css")) {
            println!("cargo:rerun-if-changed={}", entry.path().display());
        }
    }

    let _ = fs::create_dir_all(&output);

    build_js(&templates, &output);
    build_css(&templates, &output);
    copy_loader(&templates, &output);
    copy_static_assets(&templates, &output);
}

fn build_js(templates: &Path, output: &Path) {
    let mut files: Vec<_> = WalkDir::new(templates)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|e| e == "js"))
        // Exclude loader.js - it's copied separately, not bundled
        .filter(|e| e.path().file_name().is_some_and(|n| n != "loader.js"))
        .map(|e| e.path().to_path_buf())
        .collect();
    files.sort();

    if files.is_empty() {
        return;
    }

    let mut combined = String::new();
    for file in &files {
        if let Ok(content) = fs::read_to_string(file) {
            let rel = file.strip_prefix(templates).unwrap_or(file);
            combined.push_str(&format!("\n// === {} ===\n", rel.display()));
            combined.push_str(&content);
            combined.push('\n');
        }
    }

    if combined.is_empty() {
        return;
    }

    let minified = try_minify_js(&combined).unwrap_or_else(|| combined.clone());
    let hash = hex::encode(Sha256::digest(minified.as_bytes()));
    let short = &hash[..8];

    // Clean up old hash files before writing new ones
    clean_old_hash_files(output, "app.", ".min.js", short);

    let _ = fs::write(output.join(format!("app.{}.min.js", short)), &minified);
    let _ = fs::write(output.join("app.min.js"), &minified);

    if env::var("PROFILE").map_or(true, |p| p != "release") {
        let _ = fs::write(output.join("app.debug.js"), &combined);
    }

    println!("cargo:warning=Built app.min.js ({} bytes)", minified.len());
}

fn build_css(templates: &Path, output: &Path) {
    let mut combined = String::new();

    // Base styles first (from templates directory, not output)
    let base = templates.join("styles.css");
    if base.exists() {
        if let Ok(content) = fs::read_to_string(&base) {
            combined.push_str(&content);
            combined.push('\n');
        }
    }

    // Then template CSS
    let mut files: Vec<_> = WalkDir::new(templates)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|e| e == "css"))
        .map(|e| e.path().to_path_buf())
        .collect();
    files.sort();

    for file in files {
        if let Ok(content) = fs::read_to_string(&file) {
            if content.trim().is_empty() {
                continue;
            }
            let rel = file.strip_prefix(templates).unwrap_or(&file);
            combined.push_str(&format!("\n/* === {} === */\n", rel.display()));
            combined.push_str(&content);
            combined.push('\n');
        }
    }

    if combined.trim().is_empty() {
        return;
    }

    let minified = minify_css(&combined);
    let hash = hex::encode(Sha256::digest(minified.as_bytes()));
    let short = &hash[..8];

    // Clean up old hash files before writing new ones
    clean_old_hash_files(output, "styles.", ".min.css", short);

    let _ = fs::write(output.join(format!("styles.{}.min.css", short)), &minified);
    let _ = fs::write(output.join("styles.min.css"), &minified);

    println!(
        "cargo:warning=Built styles.min.css ({} bytes)",
        minified.len()
    );
}

fn try_minify_js(source: &str) -> Option<String> {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    let src = source.to_string();
    catch_unwind(AssertUnwindSafe(|| {
        let session = Session::new();
        let mut out = Vec::new();
        minify(&session, TopLevelMode::Module, src.as_bytes(), &mut out).ok()?;
        String::from_utf8(out).ok()
    }))
    .ok()?
}

fn minify_css(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut in_comment = false;
    let mut chars = css.chars().peekable();

    while let Some(c) = chars.next() {
        if in_comment {
            if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                in_comment = false;
            }
            continue;
        }
        if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            in_comment = true;
            continue;
        }
        if c.is_whitespace() {
            if !out.ends_with(|ch: char| ch.is_whitespace() || "{:;,".contains(ch))
                && chars.peek().is_some_and(|&n| !"{}:;,".contains(n))
            {
                out.push(' ');
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Remove old hash files that don't match the current hash
fn clean_old_hash_files(output: &Path, prefix: &str, suffix: &str, current_hash: &str) {
    if let Ok(entries) = fs::read_dir(output) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name();
            let name = name.to_string_lossy();

            // Check if file matches the pattern (e.g., "app.{hash}.min.js")
            // Must be long enough to contain prefix + hash + suffix
            if name.starts_with(prefix)
                && name.ends_with(suffix)
                && name.len() > prefix.len() + suffix.len()
            {
                // Extract the hash portion
                let hash_part = &name[prefix.len()..name.len() - suffix.len()];
                // Only remove if it looks like a hash (8 hex chars) and doesn't match current
                if hash_part.len() == 8
                    && hash_part.chars().all(|c| c.is_ascii_hexdigit())
                    && hash_part != current_hash
                {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }
}

/// Minifies and copies loader.js (not bundled with app.min.js)
fn copy_loader(templates: &Path, output: &Path) {
    let loader = templates.join("loader.js");
    if loader.exists() {
        if let Ok(content) = fs::read_to_string(&loader) {
            let minified = try_minify_js(&content).unwrap_or_else(|| content.clone());
            let _ = fs::write(output.join("loader.js"), &minified);
            println!("cargo:warning=Built loader.js ({} bytes)", minified.len());
        }
    }
}

/// Copies static assets (SVG, images, etc.) from templates/static to output
fn copy_static_assets(templates: &Path, output: &Path) {
    let static_dir = templates.join("static");
    if !static_dir.exists() {
        return;
    }

    // Track changes for static assets
    println!("cargo:rerun-if-changed={}", static_dir.display());

    for entry in WalkDir::new(&static_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();
        println!("cargo:rerun-if-changed={}", path.display());

        if let Some(filename) = path.file_name() {
            if let Ok(content) = fs::read(path) {
                let dest = output.join(filename);
                let _ = fs::write(&dest, &content);
                println!(
                    "cargo:warning=Copied {} ({} bytes)",
                    filename.to_string_lossy(),
                    content.len()
                );
            }
        }
    }
}
