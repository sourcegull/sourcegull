//! Code host value types shared by several domains: the `ExternalServiceKind` enum and
//! `ExternalLink`.

use async_graphql::{Enum, SimpleObject};
use sourcegull_types::ExternalServiceKind;

/// The kind of a code host connection.
#[derive(Enum, Copy, Clone, Debug, PartialEq, Eq)]
#[graphql(name = "ExternalServiceKind")]
pub enum ExternalServiceKindGql {
    /// gitlab.com or a self-managed GitLab.
    #[graphql(name = "GITLAB")]
    GitLab,
    /// github.com or GitHub Enterprise Server.
    #[graphql(name = "GITHUB")]
    GitHub,
    /// A Gitea instance.
    #[graphql(name = "GITEA")]
    Gitea,
}

impl From<ExternalServiceKindGql> for ExternalServiceKind {
    fn from(k: ExternalServiceKindGql) -> Self {
        match k {
            ExternalServiceKindGql::GitLab => ExternalServiceKind::GitLab,
            ExternalServiceKindGql::GitHub => ExternalServiceKind::GitHub,
            ExternalServiceKindGql::Gitea => ExternalServiceKind::Gitea,
        }
    }
}

impl From<ExternalServiceKind> for ExternalServiceKindGql {
    fn from(k: ExternalServiceKind) -> Self {
        match k {
            ExternalServiceKind::GitLab => ExternalServiceKindGql::GitLab,
            ExternalServiceKind::GitHub => ExternalServiceKindGql::GitHub,
            ExternalServiceKind::Gitea => ExternalServiceKindGql::Gitea,
        }
    }
}

/// A link to a page on a code host.
#[derive(SimpleObject)]
pub struct ExternalLink {
    /// The URL.
    pub url: String,
    /// The code host kind.
    pub service_kind: Option<ExternalServiceKindGql>,
    /// The code host type, e.g. `gitlab`.
    pub service_type: Option<String>,
}
