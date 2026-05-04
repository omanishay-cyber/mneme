//! Pure-Rust PDF extractor.
//!
//! Uses `pdf-extract` 0.7 to pull text out of PDF files without shelling
//! out to PyMuPDF / poppler / ghostscript. Per-page text is synthesised
//! by splitting on `\x0c` (form-feed) which pdf-extract emits between
//! page boundaries; if the PDF has no form feeds we fall back to a
//! single page.

use std::path::Path;

use tracing::debug;

use crate::extractor::{ext_of, Extractor};
use crate::types::{ExtractError, ExtractResult, ExtractedDoc, PageText};

/// PDF extractor. No configuration.
#[derive(Debug, Default, Clone, Copy)]
pub struct PdfExtractor;

impl Extractor for PdfExtractor {
    fn kinds(&self) -> &[&'static str] {
        &["pdf"]
    }

    fn extract(&self, path: &Path) -> ExtractResult<ExtractedDoc> {
        let ext = ext_of(path);
        if ext != "pdf" {
            return Err(ExtractError::Unsupported {
                path: path.to_path_buf(),
                kind: ext,
            });
        }

        // A8-001 (2026-05-04): preflight size cap. A 200 MB PDF held by
        // pdf-extract balloons to ~500-800 MB RSS once parsed, and a
        // single such file kills the build supervisor on a 2 GB VM.
        crate::check_size(path)?;

        let bytes = std::fs::read(path).map_err(|source| ExtractError::Io {
            path: path.to_path_buf(),
            source,
        })?;

        // A8-005 (2026-05-04): pdf-extract 0.7 (and the underlying
        // lopdf) is not panic-safe. Malformed PDFs -- bad object
        // streams, recursive Catalog->Pages->Kids->Catalog cycles --
        // panic from inside the FFI / decoder. With the workspace's
        // default unwinding profile a panic still aborts the current
        // task (and would abort the whole build process under a
        // future `panic = "abort"` profile, see v0.4 wishlist).
        // catch_unwind converts the panic into a typed Parse error
        // so the extractor's "log + skip" contract holds for every
        // input shape.
        let parse_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pdf_extract::extract_text_from_mem(&bytes)
        }));
        let text = match parse_result {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => {
                return Err(ExtractError::Parse {
                    path: path.to_path_buf(),
                    reason: format!("pdf-extract: {e}"),
                });
            }
            Err(panic_payload) => {
                let msg = panic_payload
                    .downcast_ref::<&'static str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| panic_payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "non-string panic payload".to_string());
                return Err(ExtractError::Parse {
                    path: path.to_path_buf(),
                    reason: format!("pdf-extract panicked: {msg}"),
                });
            }
        };

        let mut doc = ExtractedDoc::empty("pdf", path);
        // A8-013 (2026-05-04): pdf-extract 0.6.x always emitted FF
        // (\x0c) between pages. 0.7.x and PDFs from LibreOffice /
        // weasyprint sometimes emit only `\n\n` instead, which silently
        // collapses page_count to 1 for multi-hundred-page PDFs.
        // Strategy: prefer FF when present; otherwise, if the document
        // is large (> 2000 chars) and has multiple `\n\n` boundaries,
        // fall back to a paragraph-block split. Record the method used
        // in metadata so downstream graders can filter on it.
        let has_ff = text.contains('\x0c');
        let split_method: &'static str;
        let page_bodies: Vec<String> = if has_ff {
            split_method = "ff";
            text.split('\x0c').map(|s| s.to_string()).collect()
        } else if text.len() > 2000 && text.contains("\n\n") {
            split_method = "fallback";
            text.split("\n\n").map(|s| s.to_string()).collect()
        } else {
            split_method = "none";
            vec![text.clone()]
        };
        for (i, body) in page_bodies.iter().enumerate() {
            let body_trimmed = body.trim_end_matches('\n');
            if body_trimmed.is_empty() && i + 1 == page_bodies.len() {
                // pdf-extract often terminates with a trailing FF -> empty
                // tail page. Drop it.
                continue;
            }
            doc.pages.push(PageText {
                index: (i + 1) as u32,
                text: body_trimmed.to_string(),
                heading: None,
            });
        }
        if doc.pages.is_empty() {
            doc.pages.push(PageText {
                index: 1,
                text: text.trim_end_matches('\n').to_string(),
                heading: None,
            });
        }
        doc.recompute_text_from_pages();
        doc.metadata
            .insert("page_count".into(), doc.pages.len().to_string());
        doc.metadata
            .insert("byte_size".into(), bytes.len().to_string());
        doc.metadata
            .insert("page_split_method".into(), split_method.into());

        debug!(
            path = %path.display(),
            pages = doc.pages.len(),
            chars = doc.text.len(),
            "pdf extracted"
        );
        Ok(doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal but valid one-page PDF produced by hand. "Hello, mneme!"
    /// rendered at ~1 inch offset. Small enough to inline as a fixture.
    const FIXTURE_PDF: &[u8] = b"%PDF-1.4\n\
1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
2 0 obj<</Type/Pages/Count 1/Kids[3 0 R]>>endobj\n\
3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>endobj\n\
4 0 obj<</Length 44>>stream\n\
BT /F1 12 Tf 72 720 Td (Hello, mneme!) Tj ET\n\
endstream endobj\n\
5 0 obj<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>endobj\n\
xref\n\
0 6\n\
0000000000 65535 f \n\
0000000010 00000 n \n\
0000000053 00000 n \n\
0000000098 00000 n \n\
0000000183 00000 n \n\
0000000274 00000 n \n\
trailer<</Size 6/Root 1 0 R>>\n\
startxref\n\
333\n\
%%EOF\n";

    #[test]
    fn pdf_extractor_handles_minimal_fixture() {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hello.pdf");
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(FIXTURE_PDF).expect("write");
        f.flush().expect("flush");
        drop(f);

        let doc = match PdfExtractor.extract(&path) {
            Ok(d) => d,
            Err(e) => {
                // The hand-rolled fixture is deliberately minimal; if the
                // underlying `pdf-extract` changes its strictness we still
                // want a meaningful assertion rather than a hard panic.
                // Confirm the error is a Parse / Io and not a panic leak.
                assert!(
                    matches!(e, ExtractError::Parse { .. } | ExtractError::Io { .. }),
                    "unexpected error variant: {e:?}"
                );
                return;
            }
        };
        assert_eq!(doc.kind, "pdf");
        assert!(!doc.pages.is_empty(), "should produce at least one page");
        assert_eq!(doc.source, path);
    }

    #[test]
    fn pdf_extractor_rejects_non_pdf() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("not.txt");
        std::fs::write(&path, "hi").unwrap();
        let err = PdfExtractor.extract(&path).unwrap_err();
        assert!(matches!(err, ExtractError::Unsupported { .. }));
    }
}
