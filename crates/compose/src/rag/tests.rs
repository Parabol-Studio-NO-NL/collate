use super::*;

#[test]
fn four_stops_repeat_from_the_first_line_of_each_column() {
    let insets = [0.0, 24.0, 7.0, 35.0];
    let text = [LOREM; 3].join(" ");
    for composer in [Composer::Paragraph, Composer::SingleLine] {
        let attrs = ParaAttrs {
            composer: Some(composer),
            rag_pattern: Some(RagPattern::FourStop),
            rag_line_insets: Some(insets),
            word_space_max: Some(2.5),
            glyph_scale_max: Some(1.1),
            ..Default::default()
        };
        let (mut doc, story, frame) = doc_with(&text, Rect::new(0.0, 0.0, 450.0, 160.0), attrs);
        doc.item_mut(frame).unwrap().text_frame_mut().unwrap().options.columns = 2;
        let layout = compose_story(&doc, story, &ComposeOptions::default());
        let columns: Vec<_> = layout.frames[0].lines.chunk_by(|a, b| a.column == b.column).collect();
        assert_eq!(columns.len(), 2);
        assert!(!columns[0].len().is_multiple_of(4));
        for lines in columns {
            assert!(lines.len() >= 4);
            let mut met = [false; 4];
            for (i, line) in lines.iter().enumerate() {
                let gap = line.x1 - insets[i % 4] - line.end_x;
                assert!(gap >= -0.5, "{composer:?} stop {} exceeded by {gap}", i % 4);
                met[i % 4] |= gap.abs() < 0.5;
            }
            assert!(met.iter().any(|&fitted| fitted), "at least one nonfinal line fits its specified edge");
        }
    }
}

#[test]
fn near_flush_fits_only_naturally_close_nonfinal_lines() {
    let (mut doc, story, _) = doc_with(
        &[LOREM; 2].join(" "),
        Rect::new(0.0, 0.0, 190.0, 900.0),
        ParaAttrs {
            rag_pattern: Some(RagPattern::NearFlush),
            rag_near_threshold: Some(0.0),
            word_space_max: Some(1.25),
            letter_space_max: Some(0.02),
            glyph_scale_max: Some(1.02),
            ..Default::default()
        },
    );
    let before = compose_story(&doc, story, &ComposeOptions::default());
    let original = &before.frames[0].lines;
    let gaps: Vec<_> = original[..original.len() - 1].iter().map(|l| l.x1 - l.end_x).collect();
    let threshold = (gaps.iter().copied().fold(f64::INFINITY, f64::min) + gaps.iter().copied().fold(0.0, f64::max)) / 2.0;
    doc.story_mut(story).unwrap().paras[0].para.rag_near_threshold = Some(threshold);
    let after = compose_story(&doc, story, &ComposeOptions::default());
    assert_eq!(original.len(), after.frames[0].lines.len());
    let mut fitted = false;
    let mut untouched = false;
    for (a, b) in original.iter().zip(&after.frames[0].lines) {
        assert_eq!(a.range, b.range);
        if a.last_in_para || a.x1 - a.end_x > threshold {
            assert_eq!(a.end_x, b.end_x);
            untouched |= !a.last_in_para;
        } else {
            fitted |= b.end_x > a.end_x;
            assert!(b.end_x <= b.x1 + 0.01 && b.spacing <= 1.25001);
        }
    }
    assert!(fitted && untouched);
}

fn start_x(line: &Line) -> f64 {
    line.glyphs.iter().find(|g| g.visible).unwrap().x
}

#[test]
fn contextual_openings_follow_prior_text_through_recomposition() {
    let (mut doc, story, _) = doc_with(
        "Seed   \nsmall\nsmall\nsmall\n\nAfter",
        Rect::new(0.0, 0.0, 300.0, 600.0),
        ParaAttrs { rag_pattern: Some(RagPattern::ContextualIndent), ..Default::default() },
    );
    let cache = Cache::new();
    let inspect = |layout: &ComposedStory| {
        let lines = &layout.frames[0].lines;
        assert_eq!(lines.len(), 6);
        assert_eq!(start_x(&lines[0]), lines[0].x0);
        for i in 1..4 {
            let previous = lines[i - 1].glyphs.iter().rfind(|g| g.visible && !g.rendered_char.is_whitespace()).unwrap();
            assert!((start_x(&lines[i]) - previous.x).abs() < 0.01);
        }
        assert_eq!(start_x(&lines[5]), lines[5].x0, "a blank preceding paragraph resets the opening");
        start_x(&lines[3])
    };
    let initial = inspect(&cache.get(&doc, story, None));
    doc.story_mut(story).unwrap().insert(0, "Big ");
    let edited = inspect(&cache.get(&doc, story, None));
    assert!(edited > initial);
    assert_eq!(edited, inspect(&compose_story(&doc, story, &ComposeOptions::default())));
}

#[test]
fn contextual_start_resets_in_a_new_column_or_if_the_opening_cannot_fit() {
    let (mut doc, sid, frame) = doc_with(
        "Ending point\nUnbreakableopeningword\nNext",
        Rect::new(0.0, 0.0, 220.0, 220.0),
        ParaAttrs { rag_pattern: Some(RagPattern::ContextualIndent), hyphenate: Some(false), ..Default::default() },
    );
    doc.item_mut(frame).unwrap().text_frame_mut().unwrap().options.columns = 2;
    doc.story_mut(sid).unwrap().paras[2].para.start_paragraph = Some(designcraft_doc::StartParagraph::NextColumn);
    let composed = compose_story(&doc, sid, &ComposeOptions::default());
    let lines = &composed.frames[0].lines;
    for pi in [1, 2] {
        let opening = lines.iter().find(|l| l.para == pi).unwrap();
        assert_eq!(start_x(opening), opening.x0);
        assert_eq!(opening.column, u32::from(pi == 2));
    }
}

#[test]
fn contextual_start_counts_for_first_line_character_styles() {
    let (mut doc, sid, _) = doc_with(&format!("A short opening\n{LOREM}"), Rect::new(0.0, 0.0, 160.0, 600.0), ParaAttrs::default());
    let opening = &mut doc.story_mut(sid).unwrap().paras[1].para;
    opening.rag_pattern = Some(RagPattern::ContextualIndent);
    opening.nested_line_styles = Some(vec![designcraft_doc::NestedLineStyle { style: "Lead".into(), lines: 1 }]);
    std::sync::Arc::make_mut(&mut doc.styles).character.push(designcraft_doc::CharacterStyle {
        name: "Lead".into(),
        based_on: None,
        shortcut: String::new(),
        chars: designcraft_doc::CharAttrs { fill: Some("[Registration]".into()), ..Default::default() },
    });
    let composed = compose_story(&doc, sid, &ComposeOptions::default());
    let lines: Vec<_> = composed.frames[0].lines.iter().filter(|l| l.para == 1).collect();
    assert!(lines.len() > 1 && start_x(lines[0]) > lines[0].x0);
    for line in lines {
        for glyph in line.glyphs.iter().filter(|g| g.len > 0 && g.visible) {
            assert_eq!(composed.styles[glyph.style as usize].fill == "[Registration]", line.first_in_para);
        }
    }
}
