//! Pagination with opaque cursors: forward (`first` / `after`, `pageInfo { hasNextPage
//! endCursor }`) and both ways (`first`/`after` or `last`/`before`, `ConnectionPageInfo`).
//! Cursors encode an offset; clients must treat them as opaque.

use async_graphql::{Result, SimpleObject};
use sourcegull_services::relay::{marshal_id, unmarshal_int_id};

use crate::context::{codes, error};

const CURSOR_KIND: &str = "Cursor";

/// Relay-style page info.
#[derive(SimpleObject, Debug, Clone, PartialEq, Eq, Default)]
pub struct PageInfo {
    /// Whether more items follow this page.
    pub has_next_page: bool,
    /// Pass as `after` to get the next page; null on the last page.
    pub end_cursor: Option<String>,
}

impl PageInfo {
    /// Page info for a page that started at `offset`, asked for `limit` items and fetched
    /// `fetched` (callers fetch `limit + 1` to detect a next page).
    pub fn new(offset: i64, limit: i64, fetched: usize) -> Self {
        let has_next_page = i64::try_from(fetched).unwrap_or(i64::MAX) > limit;
        Self {
            has_next_page,
            end_cursor: has_next_page.then(|| encode_cursor(offset + limit)),
        }
    }
}

/// Page info for paging both ways (`ConnectionPageInfo`).
#[derive(SimpleObject, Debug, Clone, PartialEq, Eq)]
pub struct ConnectionPageInfo {
    /// Pass as `after` for the next page.
    pub end_cursor: Option<String>,
    /// Pass as `before` for the previous page.
    pub start_cursor: Option<String>,
    pub has_next_page: bool,
    pub has_previous_page: bool,
}

impl ConnectionPageInfo {
    /// Page info for the positions `start..end` of `total` items.
    pub fn new(start: i64, end: i64, total: i64) -> Self {
        let nonempty = end > start;
        Self {
            has_previous_page: start > 0,
            has_next_page: end < total,
            start_cursor: nonempty.then(|| encode_cursor(start)),
            end_cursor: nonempty.then(|| encode_cursor(end)),
        }
    }
}

/// Arguments of a connection paged both ways.
#[derive(Debug, Clone, Default)]
pub struct PageArgs {
    pub first: Option<i32>,
    pub last: Option<i32>,
    pub after: Option<String>,
    pub before: Option<String>,
}

impl PageArgs {
    /// Rejects `first` together with `last`.
    pub fn check(&self) -> Result<()> {
        if self.first.is_some() && self.last.is_some() {
            return Err(error(
                codes::INVALID_INPUT,
                "Use either first or last, not both.",
            ));
        }
        Ok(())
    }

    /// The positions `start..end` of the page among `total` items. Cursors are boundaries
    /// between positions: `after: c` starts at c, `before: c` ends there; `last` counts back
    /// from `before` (or the end).
    pub fn window(&self, total: i64, default: i64, max: i64) -> Result<(i64, i64)> {
        let size = |n: Option<i32>| n.map_or(default, |n| i64::from(n).clamp(0, max));
        if let Some(last) = self.last {
            let end = match self.before.as_deref() {
                Some(c) => decode_cursor(Some(c))?.min(total),
                None => total,
            };
            return Ok(((end - size(Some(last))).max(0), end));
        }
        let start = decode_cursor(self.after.as_deref())?.min(total);
        let mut end = (start + size(self.first)).min(total);
        if let Some(c) = self.before.as_deref() {
            end = end.min(decode_cursor(Some(c))?);
        }
        Ok((start, end.max(start)))
    }
}

/// An opaque cursor for `offset`.
pub fn encode_cursor(offset: i64) -> String {
    marshal_id(CURSOR_KIND, offset)
}

/// The offset of an `after` cursor (0 when absent).
pub fn decode_cursor(after: Option<&str>) -> Result<i64> {
    match after {
        None | Some("") => Ok(0),
        Some(c) => unmarshal_int_id(CURSOR_KIND, c)
            .filter(|n| *n >= 0)
            .ok_or_else(|| error(codes::INVALID_INPUT, format!("Invalid cursor {c:?}."))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors() {
        assert_eq!(decode_cursor(None).unwrap(), 0);
        assert_eq!(decode_cursor(Some(&encode_cursor(40))).unwrap(), 40);
        assert!(decode_cursor(Some("bogus")).is_err());
        let p = PageInfo::new(10, 5, 6);
        assert!(p.has_next_page);
        assert_eq!(decode_cursor(p.end_cursor.as_deref()).unwrap(), 15);
        assert_eq!(
            PageInfo::new(10, 5, 5),
            PageInfo {
                has_next_page: false,
                end_cursor: None
            }
        );
    }

    #[test]
    fn windows_both_ways() {
        let args = |first, last, after: Option<i64>, before: Option<i64>| PageArgs {
            first,
            last,
            after: after.map(encode_cursor),
            before: before.map(encode_cursor),
        };
        assert_eq!(
            args(None, None, None, None).window(7, 5, 10).unwrap(),
            (0, 5)
        );
        assert_eq!(
            args(Some(3), None, Some(5), None).window(7, 5, 10).unwrap(),
            (5, 7)
        );
        assert_eq!(
            args(None, Some(3), None, None).window(7, 5, 10).unwrap(),
            (4, 7)
        );
        assert_eq!(
            args(None, Some(3), None, Some(2)).window(7, 5, 10).unwrap(),
            (0, 2)
        );
        assert_eq!(
            args(Some(5), None, Some(1), Some(3))
                .window(7, 5, 10)
                .unwrap(),
            (1, 3)
        );
        assert!(args(Some(1), Some(1), None, None).check().is_err());
        let p = ConnectionPageInfo::new(4, 7, 7);
        assert!(p.has_previous_page && !p.has_next_page);
        assert_eq!(decode_cursor(p.start_cursor.as_deref()).unwrap(), 4);
        assert_eq!(ConnectionPageInfo::new(0, 0, 0).end_cursor, None);
    }
}
