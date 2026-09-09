use crate::{Capability, Tool, ToolCtx};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use wingman_core::{ToolOutcome, ToolSpec};

pub struct ReadFile;

#[derive(Debug, Deserialize)]
struct Args {
    path: String,
    #[serde(default)]
    offset: Option<u32>,
    #[serde(default)]
    limit: Option<u32>,
    /// When `true` and the file is in a supported language, returns just
    /// the signatures-only outline (one line per fn/struct/class/etc).
    /// Lets the model fit many files' shapes into one context window.
    #[serde(default)]
    summary: bool,
}

#[async_trait]
impl Tool for ReadFile {
    fn capabilities(&self) -> Capability {
        Capability::READ
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: "Read a UTF-8 text file from disk. Optional 1-based `offset` and `limit` \
                          restrict the returned line range. Set `summary: true` to get a \
                          signatures-only outline instead of the full text (supported languages: \
                          rust, python, javascript, typescript, tsx, go). Refuses files that look binary."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute or cwd-relative path." },
                    "offset": { "type": "integer", "minimum": 1, "description": "1-based starting line." },
                    "limit": { "type": "integer", "minimum": 1, "description": "Max lines to return." },
                    "summary": { "type": "boolean", "default": false, "description": "Return outline (signatures only) instead of full content." }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolOutcome {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolOutcome::err(format!("invalid args: {e}")),
        };
        let path = ctx.resolve(&args.path);
        if !ctx.allows_read(&path) {
            return ToolOutcome::err(format!(
                "read denied for {} — outside the project tree under permission mode {} \
                 (use --yolo to allow reads anywhere)",
                path.display(),
                ctx.mode()
            ));
        }
        let bytes = match ctx.fs.read(&path).await {
            Ok(b) => b,
            Err(e) => return ToolOutcome::err(format!("read {}: {e}", path.display())),
        };
        // Speculatively warm the page cache for likely-next reads (siblings)
        // and pre-warm `git status`. Fire-and-forget; never blocks this read.
        crate::prefetch::warm_siblings(path.clone());
        crate::prefetch::warm_git_status_once(ctx.project_root.clone());
        // PDFs before the binary check, because a PDF *is* binary and the
        // refusal below is otherwise the whole answer. A spec or a design doc
        // handed over as a PDF is ordinary coding context.
        let is_pdf = path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
        let bytes = if is_pdf {
            match extract_pdf_text(&bytes) {
                Ok(t) => t.into_bytes(),
                Err(e) => {
                    return ToolOutcome::err(format!("read {}: {e}", path.display()));
                }
            }
        } else {
            bytes
        };
        if !is_pdf && looks_binary(&bytes) {
            return ToolOutcome::err(format!("refusing to read binary file {}", path.display()));
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        // Jupyter notebooks: render cells as a markdown-ish layout so the
        // model sees code and prose, not raw JSON.
        let rendered = if path.extension().and_then(|s| s.to_str()) == Some("ipynb") {
            render_notebook(&text).unwrap_or(text)
        } else {
            text
        };
        let text = rendered;
        // Summary mode: short-circuit with the tree-sitter outline when
        // possible. Falls through to the full read if the language is
        // unknown so the model still gets *something* useful.
        if args.summary {
            #[cfg(feature = "treesitter")]
            {
                if let Some(lang) = wingman_ts::Language::from_path(&path) {
                    if let Some(out) = wingman_ts::outline(lang, &text) {
                        if !out.is_empty() {
                            return ToolOutcome::ok(out.trim_end().to_string());
                        }
                    }
                }
            }
        }
        let lines: Vec<&str> = text.lines().collect();
        let start = args
            .offset
            .map(|n| n.saturating_sub(1) as usize)
            .unwrap_or(0);
        let end = args
            .limit
            .map(|n| (start + n as usize).min(lines.len()))
            .unwrap_or(lines.len());
        if start >= lines.len() {
            return ToolOutcome::ok(String::new());
        }
        let slice = &lines[start..end];
        ToolOutcome::ok(slice.join("\n"))
    }
}

/// Extract a PDF's text.
///
/// Text only, deliberately. Layout, images, and form fields are dropped: what
/// the model can use from a spec is its prose, and a faithful rendering of a
/// two-column layout would cost far more context than it is worth.
///
/// A PDF with no extractable text — a scan, or pages that are one big image —
/// returns an error saying so rather than an empty string. Silence would read
/// to the model as "this document is empty", which is a worse answer than
/// "this needs OCR".
#[cfg(feature = "pdf")]
fn extract_pdf_text(bytes: &[u8]) -> Result<String, String> {
    // The library panics on some malformed documents rather than returning an
    // error, so the panic is caught and reported as the tool error it should
    // have been.
    //
    // This is not total, and the difference matters here: `catch_unwind` takes
    // unwinding panics, and a stack overflow aborts instead. RUSTSEC-2026-0187
    // was exactly that — unbounded recursion in `lopdf` on deeply nested PDF
    // objects — and no amount of wrapping at this level would have contained
    // it. The `pdf-extract` floor is set to 0.12 for that reason (it carries
    // `lopdf` >= 0.42, where the recursion is bounded); do not relax it to
    // pick up an older release.
    let extracted = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(bytes))
        .map_err(|_| "could not parse this PDF (the extractor panicked)".to_string())?
        .map_err(|e| format!("could not extract text from this PDF: {e}"))?;
    if extracted.trim().is_empty() {
        return Err(
            "this PDF has no extractable text — it is probably a scan, and would need OCR"
                .to_string(),
        );
    }
    Ok(extracted)
}

#[cfg(not(feature = "pdf"))]
fn extract_pdf_text(_bytes: &[u8]) -> Result<String, String> {
    Err("PDF support is not compiled in (build with the `pdf` feature)".to_string())
}

fn looks_binary(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(8192)];
    head.contains(&0)
}

/// Render a Jupyter `.ipynb` JSON document into a flat, model-friendly
/// markdown layout: code cells become fenced code blocks (language taken
/// from `metadata.language_info.name` or `language` per cell, fallback
/// `text`), markdown cells become their raw markdown source, and stream
/// outputs become a `> stdout:` block. Returns `None` on parse failure so
/// the caller can fall back to the raw JSON.
fn render_notebook(text: &str) -> Option<String> {
    let nb: serde_json::Value = serde_json::from_str(text).ok()?;
    let lang_global = nb
        .get("metadata")
        .and_then(|m| m.get("language_info"))
        .and_then(|l| l.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or("python")
        .to_string();
    let cells = nb.get("cells")?.as_array()?;
    let mut out = String::new();
    for (i, cell) in cells.iter().enumerate() {
        let kind = cell.get("cell_type").and_then(|v| v.as_str()).unwrap_or("");
        let source = match cell.get("source") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Array(a)) => a
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(""),
            _ => String::new(),
        };
        match kind {
            "markdown" => {
                out.push_str(&format!("<!-- cell {i}: markdown -->\n"));
                out.push_str(&source);
                if !source.ends_with('\n') {
                    out.push('\n');
                }
                out.push('\n');
            }
            "code" => {
                let lang = cell
                    .get("metadata")
                    .and_then(|m| m.get("language"))
                    .and_then(|l| l.as_str())
                    .unwrap_or(&lang_global);
                out.push_str(&format!("<!-- cell {i}: code -->\n"));
                out.push_str("```");
                out.push_str(lang);
                out.push('\n');
                out.push_str(&source);
                if !source.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("```\n");
                // Stream outputs (stdout/stderr only — skip rich displays).
                if let Some(outputs) = cell.get("outputs").and_then(|o| o.as_array()) {
                    let mut stream_text = String::new();
                    for o in outputs {
                        if o.get("output_type").and_then(|v| v.as_str()) == Some("stream") {
                            if let Some(t) = o.get("text") {
                                match t {
                                    serde_json::Value::String(s) => stream_text.push_str(s),
                                    serde_json::Value::Array(a) => {
                                        for line in a.iter().filter_map(|v| v.as_str()) {
                                            stream_text.push_str(line);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    if !stream_text.is_empty() {
                        out.push_str("> stdout:\n");
                        for line in stream_text.lines() {
                            out.push_str("> ");
                            out.push_str(line);
                            out.push('\n');
                        }
                    }
                }
                out.push('\n');
            }
            "raw" => {
                out.push_str(&format!("<!-- cell {i}: raw -->\n"));
                out.push_str(&source);
                out.push_str("\n\n");
            }
            _ => {}
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wingman_config::PermissionMode;

    #[test]
    fn renders_notebook_cells() {
        let nb = json!({
            "metadata": { "language_info": { "name": "python" } },
            "cells": [
                { "cell_type": "markdown", "source": ["# Title\n", "Hello\n"] },
                { "cell_type": "code", "source": "print(1+1)\n",
                  "outputs": [{ "output_type": "stream", "name": "stdout", "text": ["2\n"] }] },
            ]
        })
        .to_string();
        let rendered = render_notebook(&nb).unwrap();
        assert!(rendered.contains("# Title"));
        assert!(rendered.contains("```python"));
        assert!(rendered.contains("print(1+1)"));
        assert!(rendered.contains("> stdout"));
        assert!(rendered.contains("> 2"));
    }

    #[tokio::test]
    async fn read_file_renders_ipynb() {
        let dir = std::env::temp_dir().join(format!(
            "wingman-nb-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.ipynb");
        let nb = json!({
            "cells": [
                { "cell_type": "code", "source": "x = 1\n" }
            ]
        });
        std::fs::write(&path, nb.to_string()).unwrap();
        let ctx = ToolCtx::new(PermissionMode::ReadOnly, dir.clone(), dir.clone());
        let out = ReadFile
            .run(json!({ "path": path.to_string_lossy() }), &ctx)
            .await;
        assert!(!out.is_error, "got error: {}", out.content);
        assert!(out.content.contains("x = 1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn refuses_read_outside_project_tree() {
        // A secret living outside the project root.
        let outside = std::env::temp_dir().join(format!(
            "wingman-secret-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&outside, "TOP SECRET").unwrap();

        let project = std::env::temp_dir().join(format!(
            "wingman-proj-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&project).unwrap();

        // ReadOnly mode: the read must be denied (outside the tree).
        let ro = ToolCtx::new(PermissionMode::ReadOnly, project.clone(), project.clone());
        let denied = ReadFile
            .run(json!({ "path": outside.to_string_lossy() }), &ro)
            .await;
        assert!(denied.is_error);
        assert!(denied.content.contains("denied"));

        // Yolo mode: the escape hatch — read is allowed.
        let yolo = ToolCtx::new(PermissionMode::Yolo, project.clone(), project.clone());
        let allowed = ReadFile
            .run(json!({ "path": outside.to_string_lossy() }), &yolo)
            .await;
        assert!(!allowed.is_error, "got error: {}", allowed.content);
        assert!(allowed.content.contains("TOP SECRET"));

        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&project);
    }
}

#[cfg(all(test, feature = "pdf"))]
mod pdf_tests {
    use super::*;

    /// A minimal but genuinely valid one-page PDF with a single text object,
    /// built here rather than committed as a fixture so the test says exactly
    /// what it is feeding the parser.
    fn sample_pdf(body: &str) -> Vec<u8> {
        let content = format!("BT /F1 24 Tf 72 700 Td ({body}) Tj ET");
        let objs: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R                /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(),
            format!(
                "<< /Length {} >>
stream
{content}
endstream",
                content.len()
            )
            .into_bytes(),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        ];
        let mut out: Vec<u8> = b"%PDF-1.4
"
        .to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(
                format!(
                    "{} 0 obj
",
                    i + 1
                )
                .as_bytes(),
            );
            out.extend_from_slice(o);
            out.extend_from_slice(
                b"
endobj
",
            );
        }
        let xref = out.len();
        out.extend_from_slice(
            format!(
                "xref
0 {}
0000000000 65535 f 
",
                objs.len() + 1
            )
            .as_bytes(),
        );
        for off in &offsets {
            out.extend_from_slice(
                format!(
                    "{off:010} 00000 n 
"
                )
                .as_bytes(),
            );
        }
        out.extend_from_slice(
            format!(
                "trailer
<< /Size {} /Root 1 0 R >>
startxref
{xref}
%%EOF
",
                objs.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    #[test]
    fn text_comes_out_of_a_real_pdf() {
        let text = extract_pdf_text(&sample_pdf("Wingman reads PDFs")).expect("extractable");
        let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(flat.contains("Wingman reads PDFs"), "got {flat:?}");
    }

    /// Garbage in must not take the process with it. `pdf-extract` panics on
    /// some malformed input, and a tool call that aborts the agent is a much
    /// worse failure than one that returns an error.
    #[test]
    fn a_corrupt_pdf_is_an_error_not_a_panic() {
        let err = extract_pdf_text(
            b"%PDF-1.4
this is not really a pdf at all",
        )
        .expect_err("should not succeed");
        assert!(!err.is_empty());
    }

    #[test]
    fn empty_input_is_an_error() {
        assert!(extract_pdf_text(b"").is_err());
    }

    /// A scan has pages but no text layer. Reporting that plainly beats
    /// handing the model an empty string it will read as "empty document".
    #[test]
    fn a_pdf_with_no_text_layer_says_it_needs_ocr() {
        let err = extract_pdf_text(&sample_pdf("")).expect_err("no text");
        assert!(err.contains("OCR"), "got {err:?}");
    }
}
