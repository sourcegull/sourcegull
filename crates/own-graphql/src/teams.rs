//! Teams, as far as Code Ownership needs them: owners in CODEOWNERS files
//! resolve to teams by name (`@group/subgroup`), and teams can be assigned as owners.
//!
//! Types and operations: `Team`, `team(name)`,
//! `teams`, `createTeam`, `updateTeam`, `deleteTeam`, `addTeamMembers`, `setTeamMembers`,
//! `removeTeamMembers`, and `User.teams` through the `TeamMember` interface. Any signed-in
//! user may create a team (and becomes its first member); members and site admins manage
//! it; read-only teams (`readonly`, set by site admins) only site admins. Teams that exist
//! only as handles in a CODEOWNERS file are `external` teams with no ID in the database.

use async_graphql::{Context, ID, InputObject, Object, Result, SimpleObject};
use sourcegull_own::access;
use sourcegull_own::store::{ListTeams, NewTeam, OwnStore, Team};
use sourcegull_services::relay::{marshal_id, marshal_string_id, unmarshal_int_id};
use sourcegull_types::UserId;

use sourcegull_graphql_core::EmptyResponse;
use sourcegull_graphql_core::ids::parse_user_id;
use sourcegull_graphql_core::shared::{TeamMemberRef, UserRef};

use crate::context::{
    access_error, codes, db_error, error, page_size, require_viewer, services, to_int, viewer,
};
use crate::pagination::{PageInfo, decode_cursor};

/// The node kind of teams.
pub const TEAM_KIND: &str = "Team";

/// Default and maximum page sizes.
const DEFAULT_PAGE: i64 = 50;
const MAX_PAGE: i64 = 1000;

/// The ownership store of the schema.
pub fn own_store(ctx: &Context<'_>) -> Result<OwnStore> {
    Ok(OwnStore::new(&services(ctx)?.db))
}

/// Parses a `Team` node ID.
pub fn parse_team_id(id: &str) -> Result<i64> {
    unmarshal_int_id(TEAM_KIND, id)
        .ok_or_else(|| error(codes::INVALID_INPUT, format!("Invalid team ID {id:?}.")))
}

/// A team: one in the database, or an external one named by a CODEOWNERS file.
#[derive(Debug, Clone)]
pub enum TeamNode {
    Db(Team),
    External(String),
}

impl TeamNode {
    fn team(&self) -> Option<&Team> {
        match self {
            TeamNode::Db(t) => Some(t),
            TeamNode::External(_) => None,
        }
    }
}

async fn require_administer(ctx: &Context<'_>, team: &Team) -> Result<()> {
    access::require_administer(&own_store(ctx)?, viewer(ctx), team)
        .await
        .map_err(access_error)
}

#[Object(name = "Team")]
impl TeamNode {
    /// The unique ID of the team (external teams get one derived from their name).
    pub async fn id(&self) -> ID {
        ID(match self {
            TeamNode::Db(t) => marshal_id(TEAM_KIND, t.id),
            TeamNode::External(name) => marshal_string_id("ExternalTeam", name),
        })
    }

    /// The team name, unique among usernames, organization names and team names.
    async fn name(&self) -> &str {
        match self {
            TeamNode::Db(t) => &t.name,
            TeamNode::External(n) => n,
        }
    }

    /// The team's page.
    async fn url(&self) -> String {
        match self {
            TeamNode::Db(t) => format!("/teams/{}", t.name),
            TeamNode::External(_) => String::new(),
        }
    }

    /// Always null: teams have no avatar.
    #[graphql(name = "avatarURL")]
    async fn avatar_url(&self) -> Option<String> {
        None
    }

    /// A human-readable name; null if not set.
    async fn display_name(&self) -> Option<&str> {
        self.team().and_then(|t| t.display_name.as_deref())
    }

    /// Managed elsewhere: only site admins may change it.
    async fn readonly(&self) -> bool {
        self.team().is_some_and(|t| t.readonly)
    }

    /// Direct members (membership is not inherited from child teams).
    async fn members(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
        search: Option<String>,
    ) -> Result<TeamMemberConnection> {
        require_viewer(ctx)?;
        let Some(team) = self.team() else {
            return Ok(TeamMemberConnection::default());
        };
        let store = own_store(ctx)?;
        let limit = page_size(first, DEFAULT_PAGE, MAX_PAGE);
        let offset = decode_cursor(after.as_deref())?;
        let ids = store
            .list_members(team.id, search.as_deref(), limit + 1, offset)
            .await
            .map_err(db_error)?;
        let total = store
            .count_members(team.id, search.as_deref())
            .await
            .map_err(db_error)?;
        let page_info = PageInfo::new(offset, limit, ids.len());
        let mut nodes = Vec::new();
        let db = &services(ctx)?.db;
        for id in ids.into_iter().take(usize::try_from(limit).unwrap_or(0)) {
            if let Some(u) = db.users().get_by_id(id).await.map_err(db_error)? {
                nodes.push(TeamMemberRef(u));
            }
        }
        Ok(TeamMemberConnection {
            total_count: to_int(total),
            page_info,
            nodes,
        })
    }

    /// The parent team; null for a root team.
    async fn parent_team(&self, ctx: &Context<'_>) -> Result<Option<TeamNode>> {
        let Some(parent) = self.team().and_then(|t| t.parent_team_id) else {
            return Ok(None);
        };
        Ok(own_store(ctx)?
            .get_team(parent)
            .await
            .map_err(db_error)?
            .map(TeamNode::Db))
    }

    /// Direct child teams.
    async fn child_teams(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
        search: Option<String>,
    ) -> Result<TeamConnection> {
        let Some(team) = self.team() else {
            return Ok(TeamConnection::default());
        };
        list_teams(
            ctx,
            ListTeams {
                search,
                parent: Some(team.id),
                ..Default::default()
            },
            first,
            after,
        )
        .await
    }

    /// Whether the viewer may change the team.
    async fn viewer_can_administer(&self, ctx: &Context<'_>) -> Result<bool> {
        let (Some(actor), Some(team)) = (viewer(ctx), self.team()) else {
            return Ok(false);
        };
        access::can_administer(&own_store(ctx)?, actor, team)
            .await
            .map_err(db_error)
    }

    /// The creator; null once deleted.
    async fn creator(&self, ctx: &Context<'_>) -> Result<Option<UserRef>> {
        let Some(id) = self.team().and_then(|t| t.creator_id) else {
            return Ok(None);
        };
        Ok(services(ctx)?
            .db
            .users()
            .get_by_id(id)
            .await
            .map_err(db_error)?
            .map(UserRef))
    }

    /// Whether the team is not in the database (a GitLab group named in a CODEOWNERS file).
    async fn external(&self) -> bool {
        matches!(self, TeamNode::External(_))
    }

    /// Whether the team mirrors a GitLab group (`own.syncTeamsFromGitLabGroups`).
    /// Such teams are read-only and change with the next sync.
    async fn synced_from_code_host(&self) -> bool {
        self.team().is_some_and(|t| t.external_service_id.is_some())
    }
}

/// The result of `syncTeamsFromGitLabGroups`.
#[derive(SimpleObject)]
pub struct TeamSyncResult {
    /// Teams created (or adopted: a team with a group's name).
    pub created: i32,
    /// Teams renamed, re-parented or given a new display name.
    pub updated: i32,
    /// Teams deleted because their group or connection is gone.
    pub deleted: i32,
    /// Groups skipped because their name is invalid or taken.
    pub skipped: i32,
    /// Memberships of the synced teams.
    pub members: i32,
    /// Problems (unreachable code hosts, taken names).
    pub warnings: Vec<String>,
}

/// A page of teams.
#[derive(SimpleObject, Default)]
pub struct TeamConnection {
    pub total_count: i32,
    pub page_info: PageInfo,
    pub nodes: Vec<TeamNode>,
}

/// A page of team members.
#[derive(SimpleObject, Default)]
pub struct TeamMemberConnection {
    pub total_count: i32,
    pub page_info: PageInfo,
    pub nodes: Vec<TeamMemberRef>,
}

/// Lists teams with offset paging.
pub async fn list_teams(
    ctx: &Context<'_>,
    mut opts: ListTeams,
    first: Option<i32>,
    after: Option<String>,
) -> Result<TeamConnection> {
    require_viewer(ctx)?;
    let store = own_store(ctx)?;
    let limit = page_size(first, DEFAULT_PAGE, MAX_PAGE);
    let offset = decode_cursor(after.as_deref())?;
    opts.limit = limit + 1;
    opts.offset = offset;
    let teams = store.list_teams(&opts).await.map_err(db_error)?;
    let total = store.count_teams(&opts).await.map_err(db_error)?;
    let page_info = PageInfo::new(offset, limit, teams.len());
    Ok(TeamConnection {
        total_count: to_int(total),
        page_info,
        nodes: teams
            .into_iter()
            .take(usize::try_from(limit).unwrap_or(0))
            .map(TeamNode::Db)
            .collect(),
    })
}

/// `User.teams`.
pub async fn user_teams(
    ctx: &Context<'_>,
    user: UserId,
    first: Option<i32>,
    after: Option<String>,
    search: Option<String>,
) -> Result<TeamConnection> {
    list_teams(
        ctx,
        ListTeams {
            search,
            member: Some(user),
            ..Default::default()
        },
        first,
        after,
    )
    .await
}

/// Loads a team for `node(id:)` (signed-in viewers only; checked by the caller).
pub async fn load_node(ctx: &Context<'_>, id: i64) -> Result<Option<TeamNode>> {
    Ok(own_store(ctx)?
        .get_team(id)
        .await
        .map_err(db_error)?
        .map(TeamNode::Db))
}

#[derive(Default)]
pub struct TeamsQuery;

#[Object]
impl TeamsQuery {
    /// A team by name; null if not found.
    async fn team(&self, ctx: &Context<'_>, name: String) -> Result<Option<TeamNode>> {
        require_viewer(ctx)?;
        Ok(own_store(ctx)?
            .get_team_by_name(&name)
            .await
            .map_err(db_error)?
            .map(TeamNode::Db))
    }

    /// Root teams (or all with `includeChildTeams`), by name.
    async fn teams(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
        search: Option<String>,
        except_ancestor: Option<ID>,
        include_child_teams: Option<bool>,
    ) -> Result<TeamConnection> {
        let except_ancestor = except_ancestor.map(|id| parse_team_id(&id)).transpose()?;
        list_teams(
            ctx,
            ListTeams {
                search,
                roots_only: !include_child_teams.unwrap_or(false),
                except_ancestor,
                ..Default::default()
            },
            first,
            after,
        )
        .await
    }
}

/// Who to add to or remove from a team; the first set field that matches wins (user ID,
/// username, verified email, code host account).
#[derive(InputObject, Debug, Clone, Default)]
pub struct TeamMemberInput {
    #[graphql(name = "userID")]
    pub user_id: Option<ID>,
    pub username: Option<String>,
    pub email: Option<String>,
    #[graphql(name = "externalAccountServiceID")]
    pub external_account_service_id: Option<String>,
    pub external_account_service_type: Option<String>,
    #[graphql(name = "externalAccountAccountID")]
    pub external_account_account_id: Option<String>,
    pub external_account_login: Option<String>,
}

/// Resolves member inputs to users; unmatched ones fail unless `skip_unmatched`.
async fn resolve_members(
    ctx: &Context<'_>,
    members: &[TeamMemberInput],
    skip_unmatched: bool,
) -> Result<Vec<UserId>> {
    let db = &services(ctx)?.db;
    let mut out = Vec::new();
    for m in members {
        let mut found = None;
        if let Some(id) = &m.user_id {
            let id = parse_user_id(id)?;
            found = db
                .users()
                .get_by_id(id)
                .await
                .map_err(db_error)?
                .map(|u| u.id);
        }
        if found.is_none()
            && let Some(name) = m.username.as_deref()
        {
            found = db
                .users()
                .get_by_username(name)
                .await
                .map_err(db_error)?
                .map(|u| u.id);
        }
        if found.is_none()
            && let Some(email) = m.email.as_deref()
        {
            found = db
                .user_emails()
                .user_by_verified_email(email)
                .await
                .map_err(db_error)?;
        }
        if found.is_none()
            && let (Some(kind), Some(service)) = (
                m.external_account_service_type.as_deref(),
                m.external_account_service_id.as_deref(),
            )
            && (m.external_account_account_id.is_some() || m.external_account_login.is_some())
        {
            found = db
                .external_accounts(None)
                .user_by_account_or_login(
                    kind,
                    service,
                    m.external_account_account_id.as_deref(),
                    m.external_account_login.as_deref(),
                )
                .await
                .map_err(db_error)?;
        }
        match found {
            Some(u) => out.push(u),
            None if skip_unmatched => {}
            None => {
                return Err(error(
                    codes::NOT_FOUND,
                    format!("No user matches the team member {m:?}."),
                ));
            }
        }
    }
    Ok(out)
}

/// The team named by an ID or a name (exactly one must be given).
async fn find_team(ctx: &Context<'_>, id: Option<&ID>, name: Option<&str>) -> Result<Team> {
    let store = own_store(ctx)?;
    let team = match (id, name) {
        (Some(id), None) => store.get_team(parse_team_id(id)?).await.map_err(db_error)?,
        (None, Some(name)) => store.get_team_by_name(name).await.map_err(db_error)?,
        _ => {
            return Err(error(
                codes::INVALID_INPUT,
                "Specify the team by ID or by name, not both.",
            ));
        }
    };
    team.ok_or_else(|| error(codes::NOT_FOUND, "Team not found."))
}

fn team_error(e: sourcegull_db::DbError) -> async_graphql::Error {
    match e {
        sourcegull_db::DbError::AlreadyExists { .. } => error(
            codes::ALREADY_EXISTS,
            "The name is already taken by a user, an organization or a team.",
        ),
        sourcegull_db::DbError::Invalid(m) => error(codes::INVALID_INPUT, m),
        other => db_error(other),
    }
}

#[derive(Default)]
pub struct TeamsMutation;

#[Object]
impl TeamsMutation {
    /// Creates a team; the viewer becomes its first member. `readonly` needs a site admin;
    /// a child team needs membership of the parent (or a site admin).
    async fn create_team(
        &self,
        ctx: &Context<'_>,
        name: String,
        display_name: Option<String>,
        #[graphql(default)] readonly: bool,
        parent_team: Option<ID>,
        parent_team_name: Option<String>,
    ) -> Result<TeamNode> {
        let actor = access::require_may_create(viewer(ctx), readonly).map_err(access_error)?;
        let parent = match (parent_team.as_ref(), parent_team_name.as_deref()) {
            (None, None) => None,
            (id, name) => {
                let p = find_team(ctx, id, name).await?;
                require_administer(ctx, &p).await?;
                Some(p.id)
            }
        };
        let team = own_store(ctx)?
            .create_team(&NewTeam {
                name,
                display_name,
                readonly,
                parent_team_id: parent,
                creator_id: Some(actor.user_id),
            })
            .await
            .map_err(team_error)?;
        Ok(TeamNode::Db(team))
    }

    /// Updates a team's display name (`""` clears it) or parent (`makeRoot` removes it).
    #[allow(clippy::too_many_arguments)]
    async fn update_team(
        &self,
        ctx: &Context<'_>,
        id: Option<ID>,
        name: Option<String>,
        display_name: Option<String>,
        parent_team: Option<ID>,
        parent_team_name: Option<String>,
        make_root: Option<bool>,
    ) -> Result<TeamNode> {
        let team = find_team(ctx, id.as_ref(), name.as_deref()).await?;
        require_administer(ctx, &team).await?;
        let parent = match (
            parent_team.as_ref(),
            parent_team_name.as_deref(),
            make_root.unwrap_or(false),
        ) {
            (None, None, false) => None,
            (None, None, true) => Some(None),
            (id, name, false) => {
                let p = find_team(ctx, id, name).await?;
                require_administer(ctx, &p).await?;
                Some(Some(p.id))
            }
            _ => {
                return Err(error(
                    codes::INVALID_INPUT,
                    "Specify a new parent team or makeRoot, not both.",
                ));
            }
        };
        let display_name = display_name.map(|d| (!d.is_empty()).then_some(d));
        let team = own_store(ctx)?
            .update_team(team.id, display_name, parent)
            .await
            .map_err(team_error)?;
        Ok(TeamNode::Db(team))
    }

    /// Deletes a team (and its child teams).
    async fn delete_team(
        &self,
        ctx: &Context<'_>,
        id: Option<ID>,
        name: Option<String>,
    ) -> Result<EmptyResponse> {
        let team = find_team(ctx, id.as_ref(), name.as_deref()).await?;
        require_administer(ctx, &team).await?;
        own_store(ctx)?
            .delete_team(team.id)
            .await
            .map_err(db_error)?;
        Ok(EmptyResponse)
    }

    /// Adds members (existing ones are ignored).
    async fn add_team_members(
        &self,
        ctx: &Context<'_>,
        team: Option<ID>,
        team_name: Option<String>,
        members: Vec<TeamMemberInput>,
        #[graphql(default)] skip_unmatched_members: bool,
    ) -> Result<TeamNode> {
        let t = find_team(ctx, team.as_ref(), team_name.as_deref()).await?;
        require_administer(ctx, &t).await?;
        let users = resolve_members(ctx, &members, skip_unmatched_members).await?;
        own_store(ctx)?
            .add_members(t.id, &users)
            .await
            .map_err(db_error)?;
        Ok(TeamNode::Db(t))
    }

    /// Replaces the members.
    async fn set_team_members(
        &self,
        ctx: &Context<'_>,
        team: Option<ID>,
        team_name: Option<String>,
        members: Vec<TeamMemberInput>,
        #[graphql(default)] skip_unmatched_members: bool,
    ) -> Result<TeamNode> {
        let t = find_team(ctx, team.as_ref(), team_name.as_deref()).await?;
        require_administer(ctx, &t).await?;
        let users = resolve_members(ctx, &members, skip_unmatched_members).await?;
        own_store(ctx)?
            .set_members(t.id, &users)
            .await
            .map_err(db_error)?;
        Ok(TeamNode::Db(t))
    }

    /// Syncs teams from the groups of every GitLab connection now (site admins).
    /// The worker also runs it hourly while `own.syncTeamsFromGitLabGroups` is on.
    async fn sync_teams_from_git_lab_groups(&self, ctx: &Context<'_>) -> Result<TeamSyncResult> {
        crate::context::require_site_admin(ctx)?;
        let svc = services(ctx)?;
        let fence = sourcegull_db::Fence::none(sourcegull_own::teamsync::TEAM_SYNC);
        let s =
            sourcegull_own::teamsync::sync_all(&svc.db, svc.keyring.clone(), &svc.registry, &fence)
                .await
                .map_err(|e| error(codes::INTERNAL, e.to_string()))?;
        let n = |v: u64| to_int(i64::try_from(v).unwrap_or(i64::MAX));
        Ok(TeamSyncResult {
            created: n(s.created),
            updated: n(s.updated),
            deleted: n(s.deleted),
            skipped: n(s.skipped),
            members: n(s.members),
            warnings: s.warnings,
        })
    }

    /// Removes members (non-members are ignored).
    async fn remove_team_members(
        &self,
        ctx: &Context<'_>,
        team: Option<ID>,
        team_name: Option<String>,
        members: Vec<TeamMemberInput>,
        #[graphql(default)] skip_unmatched_members: bool,
    ) -> Result<TeamNode> {
        let t = find_team(ctx, team.as_ref(), team_name.as_deref()).await?;
        require_administer(ctx, &t).await?;
        let users = resolve_members(ctx, &members, skip_unmatched_members).await?;
        own_store(ctx)?
            .remove_members(t.id, &users)
            .await
            .map_err(db_error)?;
        Ok(TeamNode::Db(t))
    }
}
