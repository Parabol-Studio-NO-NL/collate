use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

use super::{Result, Session, Value, compose, floor_char_boundary, json};

/// Count the selected text, or the whole story at a caret or selected text frame.
pub(super) fn query(session: &mut Session, _: &Value) -> Result<Value> {
    let state = session.doc()?;
    let selection = &state.selection;
    if selection.cells.is_some() {
        return Ok(Value::Null);
    }
    let doc = &state.doc;
    let (id, cell, selected) = if let Some(text) = selection.text {
        (text.story, text.cell, (!text.is_caret()).then(|| text.range()))
    } else if let [frame] = selection.items.as_slice()
        && let Some(text_frame) = doc.item(*frame).and_then(designcraft_doc::Item::text_frame)
    {
        (text_frame.story, None, None)
    } else {
        return Ok(Value::Null);
    };
    let Some(story) = doc.text_story(id, cell) else { return Ok(Value::Null) };
    let range = selected.as_ref().map_or(0..story.len(), |r| floor_char_boundary(&story.text, r.start)..floor_char_boundary(&story.text, r.end));
    let text = &story.text[range.clone()];
    let paragraphs = if text.is_empty() {
        0
    } else if selected.is_some() {
        story.paras_touching(range.clone()).len()
    } else {
        story.paras.len()
    };

    let composed = session.cache.get(doc, id, None);
    let layout = match cell {
        Some(address) => match address.footnote_id() {
            Some(note) => compose::find_note(&composed, note).map(|(_, note)| note.text.as_ref()),
            None => compose::find_cell(&composed, address.table, address.row, address.col).map(|(_, _, cell)| cell.text.as_ref()),
        },
        None => Some(composed.as_ref()),
    };
    let lines = layout
        .filter(|_| !text.is_empty())
        .map_or(0, |layout| if selected.is_some() { selected_lines(layout, &story.text, range) } else { layout.line_count() });

    let scope = if selected.is_some() {
        "selection"
    } else if let Some(address) = cell {
        if address.footnote_id().is_some() { "footnote" } else { "cell" }
    } else {
        "story"
    };
    let mut codepoints = text.chars();
    let unicode: Vec<u32> = if selected.is_some() { codepoints.by_ref().take(6).map(u32::from).collect() } else { Vec::new() };
    Ok(json!({
        "scope": scope,
        "characters": text.graphemes(true).count(),
        "words": text.unicode_words().count(),
        "lines": lines,
        "paragraphs": paragraphs,
        "unicode": unicode,
        "unicodeTruncated": selected.is_some() && codepoints.next().is_some(),
    }))
}

fn selected_lines(layout: &compose::ComposedStory, text: &str, selection: Range<usize>) -> usize {
    layout
        .frames
        .iter()
        .flat_map(|frame| &frame.lines)
        .filter(|line| {
            // A paragraph return belongs to its final line. A soft wrap shares a boundary
            // with the next line but consumes no paragraph return.
            let has_return = line.last_in_para && text.as_bytes().get(line.range.end) == Some(&b'\n');
            let end = line.range.end + usize::from(has_return);
            line.range.start < selection.end && selection.start < end
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_user_perceived_characters_and_paragraphs_at_selection_boundaries() {
        let mut session = Session::new();
        session.execute("file.new", &json!({})).unwrap();
        let first = "Cafe\u{301} noir\n";
        let text = format!("{first}\nTwo words");
        let frame = session.execute("frame.create", &json!({"rect": [36, 36, 300, 200], "content": "text", "text": text, "caret": false})).unwrap();

        let counts = session.execute("type.statistics", &json!({})).unwrap();
        assert_eq!(counts["scope"], "story");
        assert_eq!(counts["characters"], 20);
        assert_eq!(counts["words"], 4);
        assert_eq!(counts["paragraphs"], 3);

        session.execute("text.select", &json!({"story": frame["story"], "anchor": first.len(), "focus": 0})).unwrap();
        let counts = session.execute("type.statistics", &json!({})).unwrap();
        assert_eq!(counts["scope"], "selection");
        assert_eq!(counts["characters"], 10);
        assert_eq!(counts["words"], 2);
        assert_eq!(counts["lines"], 1);
        assert_eq!(counts["paragraphs"], 1);
        assert_eq!(counts["unicode"][4], 0x301);
        assert_eq!(counts["unicodeTruncated"], true);

        session.execute("text.select", &json!({"story": frame["story"], "anchor": first.len(), "focus": first.len() + 1})).unwrap();
        let counts = session.execute("type.statistics", &json!({})).unwrap();
        assert_eq!(counts["words"], 0);
        assert_eq!(counts["lines"], 1);
        assert_eq!(counts["paragraphs"], 1);
    }

    #[test]
    fn lines_follow_reflow_and_a_caret_counts_the_entire_story() {
        let mut session = Session::new();
        session.execute("file.new", &json!({})).unwrap();
        let text = "A narrow frame wraps this paragraph over several lines.";
        let frame = session.execute("frame.create", &json!({"rect": [36, 36, 120, 250], "content": "text", "text": text, "caret": false})).unwrap();
        let wrapped = session.execute("type.statistics", &json!({})).unwrap();
        assert!(wrapped["lines"].as_u64().unwrap() > 1);
        assert_eq!(wrapped["paragraphs"], 1);

        session.execute("text.select", &json!({"story": frame["story"], "anchor": 3, "focus": 3})).unwrap();
        assert_eq!(session.execute("type.statistics", &json!({})).unwrap(), wrapped);
        session.execute("text.select", &json!({"story": frame["story"], "anchor": 0, "focus": 1})).unwrap();
        assert_eq!(session.execute("type.statistics", &json!({})).unwrap()["lines"], 1);
    }
}
