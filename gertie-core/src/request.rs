//! The harness-level request shape. Vendor formats are adapters in
//! gertie-providers; this is the normalized form the loop speaks.

use crate::Role;

/// A tool definition advertised to the model.
#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON schema for the parameters object.
    pub parameters: serde_json::Value,
}

/// A tool call the model wants executed, whole — deltas never leave the
/// provider layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// One message in the canonical conversation. Tool-capable conversations
/// need three shapes the old (Role, String) pair couldn't express:
/// assistant-with-tool-calls, tool-results (keyed by call id), and plain
/// turns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WireMessage {
    pub role: Role,
    pub content: Option<String>,
    /// Assistant-only: calls the model made in this turn.
    pub tool_calls: Vec<ToolCall>,
    /// Tool-only: which call this result answers.
    pub tool_call_id: Option<String>,
    /// Tool-only: the tool's name.
    pub name: Option<String>,
    /// User-only: images riding with the text, as data URLs
    /// (`data:image/png;base64,…`). Empty = text-only — the common
    /// case, and the only shape the wires emit today for other roles.
    pub images: Vec<String>,
}

impl WireMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(content.into()),
            ..Self::default()
        }
    }

    pub fn user_with_images(content: impl Into<String>, images: Vec<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(content.into()),
            images,
            ..Self::default()
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: Some(content.into()),
            ..Self::default()
        }
    }

    pub fn tool_result(call: &ToolCall, result: &str) -> Self {
        Self {
            role: Role::Tool,
            content: Some(result.to_string()),
            tool_call_id: Some(call.id.clone()),
            name: Some(call.name.clone()),
            ..Self::default()
        }
    }

    pub fn assistant_with_tool_calls(content: Option<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content,
            tool_calls,
            ..Self::default()
        }
    }
}

/// The persistence marker for an attached image: `{{image:PATH}}` rides
/// inside the stored content (the DB column is text; the bytes live in
/// the attachments dir). A load expands markers back into images.
pub const IMAGE_MARKER_PREFIX: &str = "{{image:";

/// Splits stored content into `(clean text, image paths)` — every
/// `{{image:path}}` marker is lifted out; what remains is the prose
/// around the images. An unterminated marker is kept as text: it was
/// never a marker to begin with.
pub fn extract_image_markers(content: &str) -> (String, Vec<String>) {
    let mut text = String::with_capacity(content.len());
    let mut images = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find(IMAGE_MARKER_PREFIX) {
        text.push_str(&rest[..start]);
        let tail = &rest[start + IMAGE_MARKER_PREFIX.len()..];
        match tail.find("}}") {
            Some(end) => {
                images.push(tail[..end].to_string());
                rest = &tail[end + 2..];
            }
            None => {
                text.push_str(IMAGE_MARKER_PREFIX);
                rest = tail;
                break;
            }
        }
    }
    text.push_str(rest);
    // Markers lift out and leave their surrounding gaps; collapse the
    // run down to one paragraph break.
    while text.contains("\n\n\n") {
        text = text.replace("\n\n\n", "\n\n");
    }
    (text.trim().to_string(), images)
}

/// Splits a data URL into `(media_type, base64 payload)` — what the
/// Anthropic wire wants for an image block. The OpenAI wire passes the
/// data URL through whole.
pub fn data_url_parts(data_url: &str) -> Option<(&str, &str)> {
    let rest = data_url.strip_prefix("data:")?;
    let (media_type, payload) = rest.split_once(";base64,")?;
    Some((media_type, payload))
}

/// Reads an attachment file into a data URL — the load-path inverse of
/// the send path's write. A missing or unknown-format file returns
/// None; the caller degrades to a text note.
pub fn file_to_data_url(path: &std::path::Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let media_type = match path.extension()?.to_str()? {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    };
    use base64::Engine as _;
    Some(format!(
        "data:{media_type};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

/// One model request. The loop fills this; providers translate it.
#[derive(Clone, Debug, Default)]
pub struct Request {
    /// Canonical message path, oldest first.
    pub messages: Vec<WireMessage>,
    /// Tools the model may call this turn.
    pub tools: Vec<ToolSpec>,
    /// Maximum output tokens the model should produce, when the provider
    /// supports the knob.
    pub max_output_tokens: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_lift_out_and_survive_round_trips() {
        let (text, images) =
            extract_image_markers("look at this\n\n{{image:/tmp/a b.png}}\n\nand that");
        assert_eq!(text, "look at this\n\nand that");
        assert_eq!(images, ["/tmp/a b.png"]);

        // No markers: content passes through whole.
        assert_eq!(extract_image_markers("plain words").0, "plain words");
        assert!(extract_image_markers("plain words").1.is_empty());

        // An unterminated marker is text, never a truncation.
        let (text, images) = extract_image_markers("stubborn {{image:nope");
        assert_eq!(text, "stubborn {{image:nope");
        assert!(images.is_empty());
    }

    #[test]
    fn data_urls_split_into_media_type_and_payload() {
        assert_eq!(
            data_url_parts("data:image/png;base64,QUJD"),
            Some(("image/png", "QUJD"))
        );
        // Not a data URL, or not base64-encoded: None, not a guess.
        assert_eq!(data_url_parts("text/plain"), None);
        assert_eq!(data_url_parts("data:image/png,QUJD"), None);
    }

    #[test]
    fn files_become_data_urls_by_extension() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shot.png");
        std::fs::write(&path, b"\x89PNG").unwrap();
        let url = file_to_data_url(&path).unwrap();
        let (media_type, payload) = data_url_parts(&url).unwrap();
        assert_eq!(media_type, "image/png");
        use base64::Engine as _;
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(payload)
                .unwrap(),
            b"\x89PNG"
        );
        // Unknown extension: None, not a guess.
        let mystery = dir.path().join("shot.png.txt");
        std::fs::write(&mystery, b"x").unwrap();
        assert!(file_to_data_url(&mystery).is_none());
        // Missing file: None.
        assert!(file_to_data_url(&dir.path().join("gone.png")).is_none());
    }
}
