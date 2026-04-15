use axum::extract::{Query, State};
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use chrono::{NaiveDate, TimeZone, Utc};
use epigraph_crypto::ContentHasher;
use printpdf::*;
use serde::Deserialize;
use sqlx::Row;
use uuid::Uuid;

use crate::errors::ApiError;
use crate::state::ElnState;

#[derive(Deserialize)]
pub struct ExportParams {
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub agent_id: Option<Uuid>,
    pub label: Option<String>,
}

struct ClaimEntry {
    id: Uuid,
    content: String,
    truth_value: f64,
    labels: Vec<String>,
    created_at: chrono::DateTime<Utc>,
    agent_name: String,
}

/// Wrap text at `max_chars` per line, breaking on word boundaries.
fn wrap_text(text: &str, max_chars: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        if paragraph.is_empty() {
            lines.push(String::new());
            continue;
        }
        let words: Vec<&str> = paragraph.split_whitespace().collect();
        if words.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current_line = String::new();
        for word in words {
            if current_line.is_empty() {
                current_line = word.to_string();
            } else if current_line.len() + 1 + word.len() > max_chars {
                lines.push(current_line);
                current_line = word.to_string();
            } else {
                current_line.push(' ');
                current_line.push_str(word);
            }
        }
        if !current_line.is_empty() {
            lines.push(current_line);
        }
    }
    lines
}

async fn export_notebook_pdf(
    State(state): State<ElnState>,
    Query(params): Query<ExportParams>,
) -> Result<impl IntoResponse, ApiError> {
    let from_dt = Utc
        .from_utc_datetime(&params.from.and_hms_opt(0, 0, 0).unwrap());
    let to_dt = Utc
        .from_utc_datetime(&params.to.and_hms_opt(23, 59, 59).unwrap());

    if params.from > params.to {
        return Err(ApiError::Validation(
            "'from' must be before or equal to 'to'".into(),
        ));
    }

    let rows = sqlx::query(
        r#"
        SELECT c.id, c.content, c.agent_id, c.truth_value, c.labels, c.created_at,
               COALESCE(a.display_name, c.agent_id::text) AS agent_name
        FROM claims c
        LEFT JOIN agents a ON a.id = c.agent_id
        WHERE c.created_at >= $1 AND c.created_at <= $2
          AND ($3::uuid IS NULL OR c.agent_id = $3)
          AND ($4::text IS NULL OR c.labels @> ARRAY[$4::text])
        ORDER BY c.created_at ASC
        "#,
    )
    .bind(from_dt)
    .bind(to_dt)
    .bind(params.agent_id)
    .bind(params.label.as_deref())
    .fetch_all(&state.pool)
    .await
    .map_err(|e| ApiError::Internal(format!("query failed: {e}")))?;

    let entries: Vec<ClaimEntry> = rows
        .iter()
        .map(|row| {
            let labels: Vec<String> = row.get("labels");
            ClaimEntry {
                id: row.get("id"),
                content: row.get("content"),
                truth_value: row.get("truth_value"),
                labels,
                created_at: row.get("created_at"),
                agent_name: row.get("agent_name"),
            }
        })
        .collect();

    // Build integrity hash over all content
    let mut hash_input = String::new();
    for entry in &entries {
        hash_input.push_str(&entry.id.to_string());
        hash_input.push_str(&entry.content);
        hash_input.push_str(&entry.truth_value.to_string());
        hash_input.push_str(&entry.created_at.to_rfc3339());
    }
    let content_hash = ContentHasher::hash(hash_input.as_bytes());
    let hash_hex = hex::encode(content_hash);

    // Generate PDF
    let (doc, page1, layer1) =
        PdfDocument::new("EpiScience Lab Notebook", Mm(210.0), Mm(297.0), "Layer 1");

    let font_regular = doc
        .add_builtin_font(BuiltinFont::Helvetica)
        .map_err(|e| ApiError::Internal(format!("font error: {e}")))?;
    let font_bold = doc
        .add_builtin_font(BuiltinFont::HelveticaBold)
        .map_err(|e| ApiError::Internal(format!("font error: {e}")))?;
    let font_mono = doc
        .add_builtin_font(BuiltinFont::Courier)
        .map_err(|e| ApiError::Internal(format!("font error: {e}")))?;

    let page_width = Mm(210.0);
    let margin_left = Mm(20.0);
    let margin_right = Mm(20.0);
    let margin_top = Mm(277.0); // 297 - 20
    let page_bottom = Mm(30.0);

    let mut current_page = page1;
    let mut current_layer_idx = layer1;
    let mut y = margin_top;

    let get_layer =
        |doc: &PdfDocumentReference, page: PdfPageIndex, layer: PdfLayerIndex| {
            doc.get_page(page).get_layer(layer)
        };

    let mut layer = get_layer(&doc, current_page, current_layer_idx);

    // Helper: advance y, add new page if needed
    // We'll use a macro-like approach with closures
    let line_height = Mm(5.0);

    let new_page = |doc: &PdfDocumentReference| -> (PdfPageIndex, PdfLayerIndex) {
        doc.add_page(Mm(210.0), Mm(297.0), "Layer 1")
    };

    // --- Header ---
    layer.use_text("EpiScience Lab Notebook", 16.0, margin_left, y, &font_bold);
    y -= Mm(8.0);

    let period_text = format!(
        "Period: {} to {} | Entries: {}",
        params.from,
        params.to,
        entries.len()
    );
    layer.use_text(&period_text, 10.0, margin_left, y, &font_regular);
    y -= Mm(5.0);

    let gen_time = format!("Generated: {}", Utc::now().format("%Y-%m-%d %H:%M:%S UTC"));
    layer.use_text(&gen_time, 10.0, margin_left, y, &font_regular);
    y -= Mm(7.0);

    // Separator line
    let line = Line {
        points: vec![
            (Point::new(margin_left, y), false),
            (Point::new(page_width - margin_right, y), false),
        ],
        is_closed: false,
    };
    layer.set_outline_thickness(0.5);
    layer.add_line(line);
    y -= Mm(7.0);

    // --- Entries ---
    for entry in &entries {
        // Check if we need a new page (need at least ~40mm for an entry header)
        if y < Mm(50.0) {
            let (pg, ly) = new_page(&doc);
            current_page = pg;
            current_layer_idx = ly;
            layer = get_layer(&doc, current_page, current_layer_idx);
            y = margin_top;
        }

        // Timestamp + author (bold)
        let header = format!(
            "{} — {}",
            entry.created_at.format("%Y-%m-%d %H:%M:%S UTC"),
            entry.agent_name
        );
        layer.use_text(&header, 10.0, margin_left, y, &font_bold);
        y -= line_height;

        // Claim ID + truth value + labels (mono)
        let labels_str = if entry.labels.is_empty() {
            String::from("none")
        } else {
            entry.labels.join(", ")
        };
        let meta = format!(
            "ID: {} | TV: {:.3} | Labels: {}",
            entry.id, entry.truth_value, labels_str
        );
        // Wrap meta if long
        let meta_lines = wrap_text(&meta, 90);
        for ml in &meta_lines {
            if y < page_bottom {
                let (pg, ly) = new_page(&doc);
                current_page = pg;
                current_layer_idx = ly;
                layer = get_layer(&doc, current_page, current_layer_idx);
                y = margin_top;
            }
            layer.use_text(ml, 8.0, margin_left, y, &font_mono);
            y -= Mm(4.0);
        }

        // Content (regular), wrapped
        let content_lines = wrap_text(&entry.content, 90);
        for cl in &content_lines {
            if y < page_bottom {
                let (pg, ly) = new_page(&doc);
                current_page = pg;
                current_layer_idx = ly;
                layer = get_layer(&doc, current_page, current_layer_idx);
                y = margin_top;
            }
            layer.use_text(cl, 9.0, margin_left, y, &font_regular);
            y -= Mm(4.0);
        }

        // Spacing between entries
        y -= Mm(4.0);
    }

    // --- Footer: BLAKE3 integrity hash ---
    if y < Mm(40.0) {
        let (pg, ly) = new_page(&doc);
        current_page = pg;
        current_layer_idx = ly;
        layer = get_layer(&doc, current_page, current_layer_idx);
        y = margin_top;
    }

    y -= Mm(5.0);
    let sep_line = Line {
        points: vec![
            (Point::new(margin_left, y), false),
            (Point::new(page_width - margin_right, y), false),
        ],
        is_closed: false,
    };
    layer.set_outline_thickness(0.5);
    layer.add_line(sep_line);
    y -= Mm(6.0);

    let hash_label = format!("BLAKE3 Integrity Hash: {}", hash_hex);
    layer.use_text(&hash_label, 7.0, margin_left, y, &font_mono);

    // Save PDF to bytes
    let pdf_bytes = doc
        .save_to_bytes()
        .map_err(|e| ApiError::Internal(format!("PDF generation failed: {e}")))?;

    let headers = [
        (header::CONTENT_TYPE, "application/pdf".to_string()),
        (
            header::CONTENT_DISPOSITION,
            format!(
                "attachment; filename=\"notebook_{}_{}.pdf\"",
                params.from, params.to
            ),
        ),
        (
            header::HeaderName::from_static("x-content-hash"),
            hash_hex,
        ),
    ];

    Ok((headers, pdf_bytes))
}

pub fn router(state: ElnState) -> Router {
    Router::new()
        .route("/api/v1/eln/export/notebook.pdf", get(export_notebook_pdf))
        .with_state(state)
}
