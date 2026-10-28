//! The `SearchPatternType` enum, which the search queries take and notebook query blocks
//! return.

use async_graphql::Enum;
use sourcegull_query::PatternType;

/// The pattern types (`SearchPatternType`).
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
#[graphql(rename_items = "lowercase")]
pub enum SearchPatternType {
    Standard,
    Literal,
    Regexp,
    Structural,
    Keyword,
}

impl From<SearchPatternType> for PatternType {
    fn from(t: SearchPatternType) -> PatternType {
        match t {
            SearchPatternType::Standard => PatternType::Standard,
            SearchPatternType::Literal => PatternType::Literal,
            SearchPatternType::Regexp => PatternType::Regexp,
            SearchPatternType::Structural => PatternType::Structural,
            SearchPatternType::Keyword => PatternType::Keyword,
        }
    }
}
