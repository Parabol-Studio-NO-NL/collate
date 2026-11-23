//! Bounded page edits using DesignCraft's native repagination.
//!
//! Repagination replaces all document spread IDs. The room must treat these as structural edits
//! and reject stale operations that depend on old spread IDs. The seed and canonical history must
//! give collaborating replicas the same allocator before replaying a page operation.

use std::collections::{BTreeMap, BTreeSet};

use designcraft_doc::{
    Bookmark, ButtonAction, Document, HyperlinkDest, Item, ItemId, MAX_NEXT_ID, Orientation, Page, PageId, PageSide, Spread, SpreadId,
};
use serde::{Deserialize, Serialize};

use crate::Error;

const MAX_PAGES: usize = 512;
const MAX_CHANGE: usize = 16;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "type", deny_unknown_fields)]
pub enum PageEdit {
    Append { after: PageId, count: u16, allocator: u64 },
    RemoveTrailing { pages: Vec<PageId>, allocator: u64 },
    Insert { before: Option<PageId>, count: u16, parent: Option<SpreadId>, allocator: u64 },
    Remove { pages: Vec<PageId>, allocator: u64 },
}

/// Capture a native page insertion or removal, with the top-level items that disappeared.
pub fn capture(before: &Document, after: &Document) -> Result<Option<(PageEdit, Vec<ItemId>)>, Error> {
    let old_count = before.page_count();
    let new_count = after.page_count();
    if old_count == new_count {
        return Ok(None);
    }
    if old_count.abs_diff(new_count) > MAX_CHANGE || new_count > MAX_PAGES {
        return Err(Error::Limit);
    }
    let old_pages = page_ids(before);
    let new_pages = page_ids(after);
    let allocator = before.next_id;
    let edit = if new_count > old_count {
        let at = old_pages.iter().zip(&new_pages).position(|(old, new)| old != new).unwrap_or(old_count);
        let parent = after.page(at).ok_or(Error::UnknownResource)?.parent;
        PageEdit::Insert { before: old_pages.get(at).copied(), count: (new_count - old_count) as u16, parent, allocator }
    } else {
        let retained: BTreeSet<_> = new_pages.into_iter().collect();
        let pages = old_pages.into_iter().filter(|id| !retained.contains(id)).collect();
        PageEdit::Remove { pages, allocator }
    };
    let mut projected = before.clone();
    let removed = apply(&mut projected, &edit)?;
    if projected != *after {
        return Err(Error::UnsupportedEdit);
    }
    Ok(Some((edit, removed)))
}

/// Atomically apply a confirmed page edit and report removed top-level items in page order.
pub fn apply(document: &mut Document, edit: &PageEdit) -> Result<Vec<ItemId>, Error> {
    match edit {
        PageEdit::Append { .. } | PageEdit::RemoveTrailing { .. } => {
            apply_legacy(document, edit)?;
            Ok(Vec::new())
        }
        PageEdit::Insert { .. } | PageEdit::Remove { .. } => apply_standard(document, edit),
    }
}

fn apply_standard(document: &mut Document, edit: &PageEdit) -> Result<Vec<ItemId>, Error> {
    standard_layout(document)?;
    let mut staged = document.clone();
    let mut expected_pages = page_ids(document);
    match edit {
        PageEdit::Insert { before, count, parent, allocator } => {
            insert_standard(&mut staged, &mut expected_pages, *before, usize::from(*count), *parent, *allocator)?;
        }
        PageEdit::Remove { pages, allocator } => remove_standard(&mut staged, &mut expected_pages, pages, *allocator)?,
        _ => return Err(Error::UnsupportedEdit),
    }
    standard_layout(&staged)?;
    if page_ids(&staged) != expected_pages || changed_page_references(document, &staged) {
        return Err(Error::UnsupportedEdit);
    }
    let removed = verify_page_content(document, &staged)?;
    if !preserves_shared_stacking(document, &staged) {
        return Err(Error::UnsupportedEdit);
    }
    *document = staged;
    Ok(removed)
}

fn page_ids(document: &Document) -> Vec<PageId> {
    document.pages().map(|(_, _, page)| page.id).collect()
}

fn insert_standard(
    document: &mut Document,
    expected_pages: &mut Vec<PageId>,
    before: Option<PageId>,
    count: usize,
    parent: Option<SpreadId>,
    allocator: u64,
) -> Result<(), Error> {
    validate_count(count)?;
    let old_count = expected_pages.len();
    let new_count = old_count + count;
    if new_count > MAX_PAGES {
        return Err(Error::Limit);
    }
    // At most one new spread per page, plus a temporary spread when appending.
    check_allocator(document, allocator, count + new_count + usize::from(before.is_none()))?;
    validate_parent(document, parent)?;
    let at = match before {
        Some(id) => expected_pages.iter().position(|&page| page == id).ok_or(Error::Invalid("page insertion anchor"))?,
        None => old_count,
    };
    let inserted = document.insert_pages(at.checked_sub(1), count, parent).map_err(|_| Error::UnsupportedEdit)?;
    expected_pages.splice(at..at, inserted);
    Ok(())
}

fn remove_standard(document: &mut Document, expected_pages: &mut Vec<PageId>, pages: &[PageId], allocator: u64) -> Result<(), Error> {
    validate_count(pages.len())?;
    let left = expected_pages.len().checked_sub(pages.len()).filter(|&count| count > 0).ok_or(Error::Invalid("remaining pages"))?;
    let removed: BTreeSet<_> = pages.iter().copied().collect();
    let positions: Vec<_> = expected_pages.iter().enumerate().filter_map(|(index, page)| removed.contains(page).then_some(index)).collect();
    if removed.len() != pages.len() || positions.len() != pages.len() {
        return Err(Error::Invalid("removed page identities"));
    }
    check_allocator(document, allocator, left)?;
    document.delete_pages(&positions).map_err(|_| Error::UnsupportedEdit)?;
    expected_pages.retain(|page| !removed.contains(page));
    Ok(())
}

fn standard_layout(document: &Document) -> Result<(), Error> {
    let count = document.page_count();
    if !(1..=MAX_PAGES).contains(&count) {
        return Err(Error::Limit);
    }
    if document.settings.primary_story.is_some() || document.sections.iter().any(|section| section.layout.is_some()) {
        return Err(Error::UnsupportedEdit);
    }
    let facing = document.settings.facing_pages;
    let rtl = document.settings.right_to_left_binding;
    let opening = if rtl { PageSide::Right } else { PageSide::Left };
    let sides: Vec<_> = document
        .page_numbers(count)
        .into_iter()
        .map(|number| {
            if !facing {
                PageSide::Single
            } else if (number % 2 == 1) != rtl {
                PageSide::Right
            } else {
                PageSide::Left
            }
        })
        .collect();
    let mut seen = BTreeSet::new();
    let mut index = 0;
    for spread in &document.spreads {
        if spread.parent.is_some() || !spread.allow_shuffle || !standard_spread(spread, &sides[index..], opening) {
            return Err(Error::UnsupportedEdit);
        }
        check_identity(spread.id.0, document.next_id, &mut seen)?;
        for page in &spread.pages {
            check_identity(page.id.0, document.next_id, &mut seen)?;
            validate_parent(document, page.parent)?;
        }
        index += spread.pages.len();
    }
    Ok(())
}

fn standard_spread(spread: &Spread, sides: &[PageSide], opening: PageSide) -> bool {
    let (first, second) = match sides {
        [first, second, ..] if *first == opening && *second != opening => (*first, Some(*second)),
        [first, ..] => (*first, None),
        [] => return false,
    };
    let valid = |page: &Page, side, x| {
        page.side == side && page.x == x && page.width.is_finite() && page.height.is_finite() && page.width > 0.0 && page.height > 0.0
    };
    match (spread.pages.as_slice(), second) {
        ([page], None) => valid(page, first, 0.0),
        ([left, right], Some(second)) if first == PageSide::Left => valid(left, first, 0.0) && valid(right, second, left.width),
        ([right, left], Some(second)) => valid(right, first, left.width) && valid(left, second, 0.0),
        _ => false,
    }
}

fn check_identity(id: u64, allocator: u64, seen: &mut BTreeSet<u64>) -> Result<(), Error> {
    if id == 0 || id > allocator || !seen.insert(id) { Err(Error::Invalid("page identities")) } else { Ok(()) }
}

fn validate_parent(document: &Document, parent: Option<SpreadId>) -> Result<(), Error> {
    if parent.is_some_and(|parent| document.parent_index(parent).is_none_or(|index| document.parents[index].pages.is_empty())) {
        return Err(Error::UnknownResource);
    }
    Ok(())
}

struct PageContent<'a> {
    page: &'a Page,
    items: Vec<&'a Item>,
}

fn page_content(document: &Document) -> Result<BTreeMap<PageId, PageContent<'_>>, Error> {
    let mut result = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for spread in &document.spreads {
        let mut buckets = vec![Vec::new(); spread.pages.len()];
        for item in &spread.items {
            let center = item.bounds().center().x;
            let index = spread.page_at_x(center).filter(|_| center.is_finite()).ok_or(Error::Invalid("page item position"))?;
            if !seen.insert(item.id) {
                return Err(Error::DuplicateId);
            }
            buckets[index].push(item.as_ref());
        }
        for (page, items) in spread.pages.iter().zip(buckets) {
            result.insert(page.id, PageContent { page, items });
        }
    }
    Ok(result)
}

fn verify_page_content(before: &Document, after: &Document) -> Result<Vec<ItemId>, Error> {
    let old = page_content(before)?;
    let new = page_content(after)?;
    let mut removed = Vec::new();
    for (_, _, page) in before.pages() {
        let previous = old.get(&page.id).ok_or(Error::UnknownResource)?;
        match new.get(&page.id) {
            Some(current) if retained_content(previous, current) => {}
            Some(_) => return Err(Error::UnsupportedEdit),
            None => removed.extend(previous.items.iter().map(|item| item.id)),
        }
    }
    if new.iter().any(|(id, content)| !old.contains_key(id) && (!content.items.is_empty() || !blank_page(content.page))) {
        return Err(Error::UnsupportedEdit);
    }
    Ok(removed)
}

fn preserves_shared_stacking(before: &Document, after: &Document) -> bool {
    let positions: BTreeMap<_, _> =
        after.spreads.iter().flat_map(|spread| spread.items.iter().enumerate().map(move |(index, item)| (item.id, (spread.id, index)))).collect();
    before.spreads.iter().all(|spread| {
        let mut latest = BTreeMap::new();
        spread
            .items
            .iter()
            .filter_map(|item| positions.get(&item.id))
            .all(|&(destination, index)| latest.insert(destination, index).is_none_or(|previous| previous < index))
    })
}

fn retained_content(before: &PageContent<'_>, after: &PageContent<'_>) -> bool {
    let old_x = before.page.x;
    let new_x = after.page.x;
    retained_page(before.page, after.page)
        && before.items.len() == after.items.len()
        && before.items.iter().zip(&after.items).all(|(old, new)| retained_item(old, new, old_x, new_x))
}

fn retained_page(before: &Page, after: &Page) -> bool {
    let mut expected = before.clone();
    if before.parent.is_some() && !before.local_margins {
        // Native repagination synchronizes inherited margins and columns from the new parent side.
        expected.margins = after.margins;
        expected.columns = after.columns.clone();
    } else if (before.side == PageSide::Left) != (after.side == PageSide::Left) && expected.columns.positions.is_some() {
        expected.columns = expected.columns.mirrored(after.margin_rect().width());
    }
    // Vertical guides are stored in spread coordinates. Refuse a repagination that displaces them from their page.
    for (old, new) in expected.guides.iter_mut().zip(&after.guides) {
        if old.orientation == Orientation::Vertical {
            if !same_relative_x(old.position, new.position, before.x, after.x) {
                return false;
            }
            old.position = new.position;
        }
    }
    expected.x = after.x;
    expected.side = after.side;
    expected == *after
}

fn retained_item(before: &Item, after: &Item, old_x: f64, new_x: f64) -> bool {
    let old = before.xf.as_coeffs();
    let new = after.xf.as_coeffs();
    if old[..4] != new[..4] || old[5] != new[5] || !same_relative_x(old[4], new[4], old_x, new_x) {
        return false;
    }
    let mut expected = before.clone();
    expected.xf = after.xf;
    expected == *after
}

fn same_relative_x(before: f64, after: f64, old_page_x: f64, new_page_x: f64) -> bool {
    let a = before - old_page_x;
    let b = after - new_page_x;
    // Native shuffling translates into and out of page space. Permit only the rounding from those additions.
    let scale = before.abs().max(after.abs()).max(old_page_x.abs()).max(new_page_x.abs()).max(1.0);
    a.is_finite() && b.is_finite() && (a - b).abs() <= (32.0 * f64::EPSILON) * scale
}

fn changed_page_references(before: &Document, after: &Document) -> bool {
    if before.hyperlinks.len() != after.hyperlinks.len() {
        return true;
    }
    let changed = |old, new| before.page(old).map(|page| page.id) != after.page(new).map(|page| page.id);
    let changed_link = before.hyperlinks.iter().zip(&after.hyperlinks).any(|(old, new)| match (&old.dest, &new.dest) {
        (HyperlinkDest::Page(old), HyperlinkDest::Page(new)) => changed(*old, *new),
        (HyperlinkDest::Page(_), _) | (_, HyperlinkDest::Page(_)) => true,
        _ => false,
    });
    changed_link
        || changed_bookmarks(&before.bookmarks, &after.bookmarks, changed)
        || after.placed_items().into_iter().any(|item| matches!(item.button, Some(ButtonAction::GoToPage { page }) if changed(page, page)))
}

fn changed_bookmarks(before: &[Bookmark], after: &[Bookmark], changed: impl Fn(usize, usize) -> bool) -> bool {
    let mut groups = vec![(before, after)];
    while let Some((old, new)) = groups.pop() {
        if old.len() != new.len() {
            return true;
        }
        for (old, new) in old.iter().zip(new) {
            if changed(old.page, new.page) {
                return true;
            }
            groups.push((&old.children, &new.children));
        }
    }
    false
}

fn apply_legacy(document: &mut Document, edit: &PageEdit) -> Result<(), Error> {
    shared_layout(document)?;
    let old_count = document.spreads.len();
    let mut staged = document.clone();
    let retained = match edit {
        PageEdit::Append { after, count, allocator } => {
            append(&mut staged, *after, usize::from(*count), *allocator)?;
            old_count
        }
        PageEdit::RemoveTrailing { pages, allocator } => remove(&mut staged, pages, *allocator)?,
        _ => return Err(Error::UnsupportedEdit),
    };
    if staged.sections != document.sections
        || !document.spreads[..retained].iter().zip(&staged.spreads).all(|(before, after)| unchanged_spread(before, after))
    {
        return Err(Error::UnsupportedEdit);
    }
    *document = staged;
    Ok(())
}

fn append(document: &mut Document, after: PageId, count: usize, allocator: u64) -> Result<(), Error> {
    validate_count(count)?;
    let old_count = document.spreads.len();
    let new_count = old_count + count;
    if new_count > MAX_PAGES {
        return Err(Error::Limit);
    }
    // The native path allocates page IDs, a temporary append spread, and one repaginated spread per page.
    check_allocator(document, allocator, count + 1 + new_count)?;
    let tail = document.page(old_count - 1).filter(|page| page.id == after).ok_or(Error::Invalid("page tail"))?;
    document.insert_pages(Some(old_count - 1), count, tail.parent).map_err(|_| Error::UnsupportedEdit)?;
    if document.spreads.len() != new_count || !document.spreads[old_count..].iter().all(|spread| is_blank(spread)) {
        return Err(Error::UnsupportedEdit);
    }
    Ok(())
}

fn remove(document: &mut Document, pages: &[PageId], allocator: u64) -> Result<usize, Error> {
    validate_count(pages.len())?;
    let old_count = document.spreads.len();
    let keep = old_count.checked_sub(pages.len()).filter(|&count| count > 0).ok_or(Error::Invalid("remaining pages"))?;
    check_allocator(document, allocator, keep)?;
    let trailing = &document.spreads[keep..];
    if trailing.iter().zip(pages).any(|(spread, id)| spread.pages.first().is_none_or(|page| page.id != *id)) {
        return Err(Error::Invalid("page tail"));
    }
    if !trailing.iter().all(|spread| is_blank(spread)) || has_page_references(document, keep) {
        return Err(Error::UnsupportedEdit);
    }
    document.delete_pages(&(keep..old_count).collect::<Vec<_>>()).map_err(|_| Error::UnsupportedEdit)?;
    if document.spreads.len() != keep {
        return Err(Error::UnsupportedEdit);
    }
    Ok(keep)
}

fn shared_layout(document: &Document) -> Result<(), Error> {
    if document.spreads.is_empty() || document.spreads.len() > MAX_PAGES {
        return Err(Error::Limit);
    }
    if document.settings.facing_pages || document.settings.primary_story.is_some() || document.sections.iter().any(|section| section.layout.is_some())
    {
        return Err(Error::UnsupportedEdit);
    }
    let mut ids = BTreeSet::new();
    for spread in &document.spreads {
        let [page] = spread.pages.as_slice() else { return Err(Error::UnsupportedEdit) };
        if spread.parent.is_some() || !spread.allow_shuffle || page.side != PageSide::Single || page.x != 0.0 {
            return Err(Error::UnsupportedEdit);
        }
        if [spread.id.0, page.id.0].into_iter().any(|id| id == 0 || id > document.next_id || !ids.insert(id)) {
            return Err(Error::Invalid("page identities"));
        }
        if let Some(parent) = page.parent
            && document.parent_index(parent).is_none_or(|index| document.parents[index].pages.is_empty())
        {
            return Err(Error::UnknownResource);
        }
    }
    Ok(())
}

fn validate_count(count: usize) -> Result<(), Error> {
    match count {
        0 => Err(Error::Invalid("page count")),
        1..=MAX_CHANGE => Ok(()),
        _ => Err(Error::Limit),
    }
}

fn check_allocator(document: &Document, expected: u64, needed: usize) -> Result<(), Error> {
    if document.next_id != expected {
        return Err(Error::Invalid("page allocator"));
    }
    if expected.checked_add(needed as u64).is_none_or(|next| next > MAX_NEXT_ID) {
        return Err(Error::Limit);
    }
    Ok(())
}

fn unchanged_spread(a: &Spread, b: &Spread) -> bool {
    a.pages == b.pages && a.items == b.items && a.parent == b.parent && a.allow_shuffle == b.allow_shuffle
}

fn is_blank(spread: &Spread) -> bool {
    spread.items.is_empty() && matches!(spread.pages.as_slice(), [page] if blank_page(page))
}

fn blank_page(page: &Page) -> bool {
    page.guides.is_empty()
        && page.overridden.is_empty()
        && !page.local_margins
        && page.liquid.is_off()
        && page.transition.is_none()
        && page.view_rotation == 0
        && page.show_parent_items
        && !page.show_parent_overlay
}

fn has_page_references(document: &Document, first_removed: usize) -> bool {
    document.sections.iter().any(|section| section.start >= first_removed)
        || document.hyperlinks.iter().any(|link| matches!(link.dest, HyperlinkDest::Page(page) if page >= first_removed))
        || has_bookmarks(&document.bookmarks, first_removed)
}

fn has_bookmarks(bookmarks: &[Bookmark], first_removed: usize) -> bool {
    let mut groups = vec![bookmarks];
    while let Some(group) = groups.pop() {
        for bookmark in group {
            if bookmark.page >= first_removed {
                return true;
            }
            if !bookmark.children.is_empty() {
                groups.push(&bookmark.children);
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use designcraft_doc::{
        Bookmark, ButtonAction, Document, Hyperlink, HyperlinkDest, HyperlinkSource, Item, ItemId, MAX_NEXT_ID, Margins, PageId, PageSide, Shape,
        SpreadRef,
        build::NewDocument,
        geom::{Rect, shapes},
    };

    use crate::Error;

    use super::{PageEdit, apply, capture};

    fn document(pages: usize) -> Document {
        Document::new(&NewDocument { pages, facing_pages: false, ..Default::default() })
    }

    fn add_frame(document: &mut Document, page: usize) -> ItemId {
        add_frame_at(document, page, 10.0)
    }

    fn add_frame_at(document: &mut Document, page: usize, x: f64) -> ItemId {
        let (spread, _) = document.page_loc(page).unwrap();
        let x = x + document.page(page).unwrap().x;
        let id = ItemId(document.alloc());
        let path = shapes::rectangle(Rect::new(x, 10.0, x + 30.0, 60.0));
        document.insert_item(SpreadRef::Doc(spread), Item::new(id, document.default_layer(), Shape::Rectangle, path), None).unwrap();
        id
    }

    #[test]
    fn version_five_append_and_removal_still_replay_exactly() {
        let mut original = document(2);
        add_frame(&mut original, 0);
        let tail = original.page(1).unwrap();
        let mut native = original.clone();
        native.insert_pages(Some(1), 2, tail.parent).unwrap();
        let append = PageEdit::Append { after: tail.id, count: 2, allocator: original.next_id };
        let append = serde_json::from_slice(&serde_json::to_vec(&append).unwrap()).unwrap();
        let mut peer = original.clone();
        assert!(apply(&mut peer, &append).unwrap().is_empty());
        assert_eq!(peer, native);
        assert_eq!(peer.page(2).unwrap().parent, tail.parent);
        assert_eq!(peer.page(1).unwrap().id, tail.id);
        assert_eq!(peer.spreads[0].items, original.spreads[0].items);

        let mut shortened = native.clone();
        shortened.delete_pages(&[2, 3]).unwrap();
        let pages = [native.page(2).unwrap().id, native.page(3).unwrap().id].to_vec();
        let remove = PageEdit::RemoveTrailing { pages, allocator: native.next_id };
        assert!(apply(&mut peer, &remove).unwrap().is_empty());
        assert_eq!(peer, shortened);
        assert_eq!(peer.page(1).unwrap().id, tail.id);
        peer.check().unwrap();
    }

    #[test]
    fn removal_preserves_trailing_page_content_and_nested_bookmark_targets() {
        let mut original = document(3);
        let tail = original.page(2).unwrap().id;
        original.bookmarks.push(Bookmark {
            name: "Overview".into(),
            page: 0,
            children: vec![Bookmark { name: "Appendix".into(), page: 2, children: vec![], generated_by_toc: false }],
            generated_by_toc: false,
        });
        let expected = original.clone();
        let remove = PageEdit::RemoveTrailing { pages: vec![tail], allocator: original.next_id };
        assert_eq!(apply(&mut original, &remove), Err(Error::UnsupportedEdit));
        assert_eq!(original, expected);

        original.bookmarks.clear();
        add_frame(&mut original, 2);
        let expected = original.clone();
        let remove = PageEdit::RemoveTrailing { pages: vec![tail], allocator: original.next_id };
        assert_eq!(apply(&mut original, &remove), Err(Error::UnsupportedEdit));
        assert_eq!(original, expected);
    }

    #[test]
    fn captures_only_exact_native_operations_with_compatible_allocator() {
        let original = document(3);
        let tail = original.page(2).unwrap();
        let mut mixed = original.clone();
        mixed.insert_pages(Some(2), 1, tail.parent).unwrap();
        mixed.title = "Unexpected edit".into();
        assert_eq!(capture(&original, &mixed), Err(Error::UnsupportedEdit));

        let mut peer = original.clone();
        let stale = PageEdit::Insert { before: None, count: 1, parent: tail.parent, allocator: peer.next_id - 1 };
        assert_eq!(apply(&mut peer, &stale), Err(Error::Invalid("page allocator")));
        let wrong_tail = PageEdit::RemoveTrailing { pages: vec![PageId(0)], allocator: peer.next_id };
        assert_eq!(apply(&mut peer, &wrong_tail), Err(Error::Invalid("page tail")));
        assert_eq!(peer, original);
        peer.next_id = MAX_NEXT_ID;
        let bounded = PageEdit::Insert { before: None, count: 1, parent: tail.parent, allocator: peer.next_id };
        assert_eq!(apply(&mut peer, &bounded), Err(Error::Limit));
        assert_eq!(peer.page_count(), original.page_count());
    }

    #[test]
    fn inserting_on_facing_pages_preserves_items_when_the_page_changes_sides() {
        for rtl in [false, true] {
            let mut original = Document::new(&NewDocument { pages: 3, ..Default::default() });
            original.settings.right_to_left_binding = rtl;
            for page in &mut std::sync::Arc::make_mut(&mut original.parents[0]).pages {
                page.margins = Margins::uniform(if page.side == PageSide::Left { 42.0 } else { 54.0 });
            }
            original.repaginate();
            let first = original.page(0).unwrap();
            let (first_id, parent) = (first.id, first.parent);
            let original_side = first.side;
            let frames = [add_frame(&mut original, 0), add_frame_at(&mut original, 0, 12.4)];
            let mut native = original.clone();
            let new_ids = native.insert_pages(None, 1, parent).unwrap();
            let candidate = PageEdit::Insert { before: Some(first_id), count: 1, parent, allocator: original.next_id };
            assert_eq!(capture(&original, &native).unwrap(), Some((candidate.clone(), Vec::new())));
            let mut peer = original.clone();
            let wire = serde_json::to_vec(&candidate).unwrap();
            assert!(apply(&mut peer, &serde_json::from_slice(&wire).unwrap()).unwrap().is_empty());
            assert_eq!(peer, native);
            assert_eq!(peer.page(0).unwrap().id, new_ids[0]);
            let retained = peer.page(1).unwrap();
            let (spread, _) = peer.page_loc(1).unwrap();
            assert_eq!(retained.id, first_id);
            assert_ne!(retained.side, original_side);
            assert_ne!(retained.margins, original.page(0).unwrap().margins);
            assert_eq!(peer.spreads[spread].items.iter().map(|item| item.id).collect::<Vec<_>>(), frames);
            peer.check().unwrap();
        }
    }

    #[test]
    fn uneven_facing_removal_reports_occupied_leaves_and_preserves_the_other_frames() {
        let mut original = Document::new(&NewDocument { pages: 6, ..Default::default() });
        let survivors = [add_frame(&mut original, 0), add_frame_at(&mut original, 2, 12.4), add_frame(&mut original, 4)];
        let occupied = [add_frame(&mut original, 1), add_frame(&mut original, 3)];
        let removed_pages = [original.page(1).unwrap().id, original.page(3).unwrap().id].to_vec();
        let retained_pages: Vec<_> = original.pages().filter_map(|(_, _, page)| (!removed_pages.contains(&page.id)).then_some(page.id)).collect();
        let mut native = original.clone();
        native.delete_pages(&[1, 3]).unwrap();
        let (edit, removed) = capture(&original, &native).unwrap().unwrap();
        assert_eq!(edit, PageEdit::Remove { pages: removed_pages, allocator: original.next_id });
        assert_eq!(removed, occupied);
        let mut peer = original;
        assert_eq!(apply(&mut peer, &edit).unwrap(), occupied);
        assert_eq!(peer, native);
        assert_eq!(super::page_ids(&peer), retained_pages);
        assert!(survivors.iter().all(|&id| peer.item(id).is_some()));
        assert!(occupied.iter().all(|&id| peer.item(id).is_none()));
        peer.check().unwrap();
    }

    #[test]
    fn page_changes_reject_retargeting_absolute_page_references() {
        let mut original = document(3);
        let frame = add_frame(&mut original, 0);
        let button_id = ItemId(original.alloc());
        let first = original.page(0).unwrap();
        let insert = PageEdit::Insert { before: Some(first.id), count: 1, parent: first.parent, allocator: original.next_id };
        let remove = PageEdit::Remove { pages: vec![original.page(1).unwrap().id], allocator: original.next_id };
        let mut linked = original.clone();
        linked.hyperlinks.push(Hyperlink {
            id: 1,
            name: "Appendix".into(),
            source: HyperlinkSource::Item { id: frame },
            dest: HyperlinkDest::Page(2),
        });
        let mut bookmarked = original.clone();
        bookmarked.bookmarks.push(Bookmark {
            name: "Overview".into(),
            page: 0,
            children: vec![Bookmark { name: "Appendix".into(), page: 2, children: vec![], generated_by_toc: false }],
            generated_by_toc: false,
        });
        let mut interactive = original;
        let mut button = Item::new(button_id, interactive.default_layer(), Shape::Rectangle, shapes::rectangle(Rect::new(0.0, 0.0, 20.0, 40.0)));
        button.button = Some(ButtonAction::GoToPage { page: 2 });
        interactive.insert_item(SpreadRef::Parent(0), button, None).unwrap();
        for mut peer in [linked, bookmarked, interactive] {
            let expected = peer.clone();
            for edit in [&insert, &remove] {
                assert_eq!(apply(&mut peer, edit), Err(Error::UnsupportedEdit));
                assert_eq!(peer, expected);
            }
            let append = PageEdit::Insert { before: None, count: 1, parent: peer.page(2).unwrap().parent, allocator: peer.next_id };
            assert!(apply(&mut peer, &append).unwrap().is_empty());
        }
    }

    #[test]
    fn appending_a_facing_page_cannot_claim_an_existing_pasteboard_frame() {
        let mut original = Document::new(&NewDocument { pages: 2, ..Default::default() });
        let tail = original.page(1).unwrap();
        let (width, parent) = (tail.width, tail.parent);
        let existing = add_frame_at(&mut original, 1, width + 5.0);
        let mut native = original.clone();
        native.insert_pages(Some(1), 1, parent).unwrap();
        assert!(native.page_has_items(2), "native geometry places the former pasteboard frame on the new page");
        assert!(native.item(existing).is_some());
        assert_eq!(capture(&original, &native), Err(Error::UnsupportedEdit));
        let mut peer = original.clone();
        let edit = PageEdit::Insert { before: None, count: 1, parent, allocator: original.next_id };
        assert_eq!(apply(&mut peer, &edit), Err(Error::UnsupportedEdit));
        assert_eq!(peer, original);
    }

    #[test]
    fn retained_frames_keep_their_stacking_when_they_still_share_a_spread() {
        let mut original = Document::new(&NewDocument { pages: 3, ..Default::default() });
        let tail = original.page(2).unwrap();
        let (width, parent) = (tail.width, tail.parent);
        let right = add_frame_at(&mut original, 2, -10.0);
        let left = add_frame_at(&mut original, 1, width - 20.0);
        let order = |document: &Document| document.spreads[1].items.iter().map(|item| item.id).collect::<Vec<_>>();
        assert_eq!(order(&original), [right, left]);
        let mut appended = original.clone();
        appended.insert_pages(Some(2), 1, parent).unwrap();
        assert_eq!(order(&appended), [left, right]);
        assert_eq!(capture(&original, &appended), Err(Error::UnsupportedEdit));

        let mut split = original.clone();
        split.insert_pages(None, 1, parent).unwrap();
        assert_ne!(split.find(left).unwrap().spread, split.find(right).unwrap().spread);
        let (edit, removed) = capture(&original, &split).unwrap().unwrap();
        assert!(removed.is_empty());
        assert!(apply(&mut original, &edit).unwrap().is_empty());
        assert_eq!(original, split);
    }
}
