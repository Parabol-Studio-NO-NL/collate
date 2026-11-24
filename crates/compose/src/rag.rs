//! Native paragraph rag geometry. All coordinates are in the composer's current text slot.
use super::{Align, BreakInput, FrameText, Line, ParaProps, RagPattern, breaker, page_alignment};

pub(super) fn rag_inset(pp: &ParaProps, line_in_column: usize, measure: f64) -> f64 {
    let inset = match pp.rag_pattern {
        RagPattern::Natural | RagPattern::Stepped if !line_in_column.is_multiple_of(2) => pp.rag_inset,
        RagPattern::AlternatingEdges => pp.rag_inset,
        RagPattern::FourStop => pp.rag_line_insets[line_in_column % pp.rag_line_insets.len()],
        _ => 0.0,
    };
    if inset.is_finite() { inset.clamp(0.0, (measure - 12.0).max(0.0)) } else { 0.0 }
}

pub(super) fn rag_line_bounds(pp: &ParaProps, line: usize, x0: f64, x1: f64, left_page: bool) -> (f64, f64, Align) {
    let inset = rag_inset(pp, line, x1 - x0);
    match pp.rag_pattern {
        RagPattern::None => (x0, x1, page_alignment(pp.align, left_page)),
        RagPattern::Stepped | RagPattern::FourStop => (x0, x1 - inset, Align::LeftJustified),
        RagPattern::AlternatingEdges if !line.is_multiple_of(2) => (x0 + inset, x1, Align::Right),
        _ => (x0, x1 - inset, Align::Left),
    }
}

/// A paragraph can borrow its predecessor's final visible character only in the same column.
pub(super) fn previous_character(line: Option<&Line>, paragraph: usize, column: usize) -> Option<f64> {
    let line = line.filter(|line| line.column as usize == column && paragraph.checked_sub(1) == Some(line.para) && line.last_in_para)?;
    line.glyphs.iter().rev().find(|g| g.visible && g.len > 0 && !g.rendered_char.is_whitespace()).map(|g| g.x)
}

pub(super) fn contextual_indent(anchor: Option<f64>, left: f64, right: f64, opening_width: f64) -> f64 {
    anchor.filter(|&x| x > left && x + opening_width <= right).map_or(0.0, |x| x - left)
}

/// Recalculate the opening when the breaker tries the first line in a new slot or column.
pub(super) fn opening_props(input: &BreakInput<'_>, previous: Option<&Line>, paragraph: usize, column: usize) -> Option<ParaProps> {
    let pp = input.props;
    if input.line_no != 0 || pp.rag_pattern != RagPattern::ContextualIndent {
        return None;
    }
    let (x0, x1) = input.slots.first().copied().unwrap_or((input.column.x0, input.column.x1));
    let opening = breaker::first_word_width(input.glyphs, input.hyphens, input.spacing);
    Some(ParaProps {
        first_line_indent: contextual_indent(previous_character(previous, paragraph, column), x0 + pp.left_indent, x1 - pp.right_indent, opening),
        ..pp.clone()
    })
}

/// Nested line styles estimate their first line during shaping, before final placement.
pub(super) fn nested_anchor(pp: &ParaProps, frame: Option<&FrameText>, paragraph: usize, column: usize) -> Option<f64> {
    if pp.rag_pattern != RagPattern::ContextualIndent || pp.nested_line_styles.is_empty() {
        return None;
    }
    previous_character(frame?.lines.last(), paragraph, column)
}
