# Permissions and sign-in

## Sign-in providers

Sign-in methods are listed in `auth.providers` in the site configuration (Site admin → Site
configuration). Several can be active at once; the sign-in page shows one button per external
provider. The first account created on a new instance becomes the site admin.

### Built-in accounts

```jsonc
{ "type": "builtin", "allowSignup": false }
```

Usernames and passwords stored in Sourcegull. With `allowSignup: false` (the default) only site
admins create accounts, or visitors [request access](#access-requests). Related settings:

- `auth.passwordPolicy`: minimum length (default 12), and optionally digits, mixed case and
  special characters.
- `auth.lockout`: lock an account after `failedAttemptThreshold` failed sign-ins (default 5)
  within `consecutivePeriod` seconds of the first failure (default an hour), for `lockoutPeriod`
  seconds (default 30 minutes). Failures and locks count per account and client address (IPv6
  addresses per /64 network): failures from one address lock the account only for that
  address, so a stranger cannot lock out its owner. A failure after the period starts a new
  count at 1; a successful sign-in clears the address's count. A locked account, an unknown
  account and a wrong password get the same answer.
- `auth.rateLimits`: request limits on the endpoints anyone can call, shared by every process
  (counted in Postgres). Over a limit the endpoint answers `429` with `Retry-After` until the
  window ends. Each limit is `{"requests": n, "period": seconds}`; `"enabled": false` turns
  them all off.

  | Key | Counts | Default |
  |---|---|---|
  | `signInPerAddress` | password sign-in attempts per client address | 50 per 5 min |
  | `failedSignInsPerAccount` | failed sign-ins per username or email, from any address | 100 per hour |
  | `passwordResetPerAddress` | password reset requests per client address | 10 per hour |
  | `passwordResetPerEmail` | password reset requests per email address | 3 per hour |
  | `accessRequestPerAddress` | [access requests](#access-requests) per client address | 10 per hour |
  | `accessRequestPerEmail` | access requests per email address | 3 per hour |
  | `oauthAuthorizePerAddress` | `/.oauth/authorize` requests per client address | 300 per 5 min |
  | `failedWebhooksPerAddress` | rejected incoming code host webhook deliveries (4xx) per client address | 100 per 5 min |

  Per-account and per-email limits count the name as typed (ignoring case), whether or not it
  exists. The client address is the TCP peer, or, when the peer is listed in
  `auth.allowedIpAddress.trustedClientIpAddress`, the address it reports in
  `clientIpRequestHeader` (default `X-Forwarded-For`), also while the allowlist itself is off.
  Behind a reverse proxy, list it there; otherwise every request counts against the proxy's
  address.
- `auth.sessionExpiry`: session lifetime (default `30d`).
- Password reset and email verification links are emailed when `email.smtp` is configured.
  Without it, the forgot-password page and resending a verification link say that email is not
  set up, and site admins create reset links under Site admin > Users > Reset password. A reset
  request is answered at once, the same for known and unknown addresses; the lookup and the email
  happen in the background.
- Completing a password reset ends the user's sessions, deletes their access tokens and revokes
  the OAuth grants of their connected apps.

#### Locked-out admins

When no site admin can sign in and email is not configured, run this where the server's
environment is set, for example in a web container (it reads `SOURCEGULL_DATABASE_URL`, and
`SOURCEGULL_SITE_CONFIG_FILE` and `SOURCEGULL_EXTERNAL_URL` when set):

```sh
# Kubernetes
kubectl exec deploy/<release>-web -- sourcegull users reset-password <username>
# Docker Compose
docker compose exec web sourcegull users reset-password <username>
```

It prints a single-use reset link that expires after `auth.passwordResetLinkExpiry`, replaces any
earlier unused link for the user, and clears the user's sign-in lockouts. Open the link in a
browser and set a new password.

### GitLab OAuth

```jsonc
{
  "type": "gitlab",
  "url": "https://gitlab.example.com/",
  "clientID": "<application id>",
  "clientSecret": "<secret>",
  "allowSignup": true,
  "allowGroups": ["acme", "acme/platform"],
  "displayName": "GitLab"
}
```

1. Set `externalURL` first; the callback URL is built from it.
2. In GitLab, create an OAuth application (user, group or, on self-managed GitLab,
   instance-wide under Admin → Applications) with the redirect URI
   `<externalURL>/.auth/gitlab/callback`, **Confidential** checked, and the scopes `read_user`
   and `read_api`.
3. Copy its Application ID and Secret into `clientID` and `clientSecret`.

`allowSignup` creates a Sourcegull account on first sign-in; without it, GitLab users can only sign
in to an existing account with the same verified email (see [linking by
email](#linking-by-email)). `allowGroups` admits only members of those groups. Signing in with
GitLab also lets Sourcegull apply the user's
[GitLab repository permissions](#repository-permissions).

#### Linking by email

The first sign-in through a code host or OpenID Connect provider links the account to the
existing Sourcegull user with the same email, when the provider says the email is verified. Site
admins are linked the same way. Set `linkByEmail: false` on a provider to turn this off: a first
sign-in whose verified email belongs to an existing user then fails with "A user with the email
address of this account already exists" (a new user cannot take an email another user has
verified), and the user signs in another way.

Linking trusts the provider's email verification. On a self-managed code host that does not
require email confirmation, anyone who can register there can claim any address, and so sign
in as the Sourcegull user with that address, a site admin included. Set `linkByEmail: false` for
such a host.

### GitHub OAuth

```jsonc
{
  "type": "github",
  "url": "https://github.com/",
  "clientID": "<client id>",
  "clientSecret": "<client secret>",
  "allowSignup": true,
  "allowOrgs": ["acme"],
  "displayName": "GitHub"
}
```

1. Set `externalURL` first; the callback URL is built from it.
2. In GitHub, register an OAuth app (Settings → Developer settings → OAuth Apps, for your
   account or, preferably, an organization) with the authorization callback URL
   `<externalURL>/.auth/github/callback`. Use an OAuth app, not a GitHub App.
3. Copy its Client ID and a generated client secret into `clientID` and `clientSecret`. For
   GitHub Enterprise Server, set `url` to the instance (`https://github.example.com/`).

Sourcegull requests the scopes `read:user` and `user:email`, plus `read:org` when `allowOrgs` is
set and `repo` when a GitHub connection for the same URL has an `authorization` block
([repository permissions](#github-repository-permissions)). The account's email is the primary email when GitHub has verified it. `allowSignup` and
the matching of existing accounts work as for GitLab; `allowOrgs` admits only members of those
organizations (logins, any case). An organization that restricts OAuth app access must approve
the app, or its members are not seen as members.

### Gitea OAuth

```jsonc
{
  "type": "gitea",
  "url": "https://gitea.example.com/",
  "clientID": "<client id>",
  "clientSecret": "<client secret>",
  "allowSignup": true,
  "allowOrgs": ["acme"],
  "displayName": "Gitea"
}
```

1. Set `externalURL` first; the callback URL is built from it.
2. In Gitea, create an OAuth2 application (Site Administration → Applications for the whole
   instance, or Settings → Applications for your account) with the redirect URI
   `<externalURL>/.auth/gitea/callback` and **Confidential client** checked.
3. Copy its Client ID and Client Secret into `clientID` and `clientSecret`, and set `url` to
   the instance (required).

Sourcegull requests the scopes `read:user` and `read:organization`. The account's email is the
primary email when Gitea has verified it. `allowSignup` and the matching of existing accounts
work as for GitLab; `allowOrgs` admits only members of those organizations. When a Gitea
connection for the same `url` has an [`authorization` block](#repository-permissions), Sourcegull
also requests `read:repository`, so the permissions sync can read the user's repositories with
their token; users who signed in before it was added sign in again to grant it. Gitea access
tokens expire (an hour by default); the permissions sync refreshes them with this provider.

### OpenID Connect

```jsonc
{
  "type": "openidconnect",
  "issuer": "https://idp.example.com/realms/acme",
  "clientID": "sourcegull",
  "clientSecret": "<secret>",
  "requireEmailDomain": "example.com",
  "allowSignup": true,
  "allowGroups": ["engineering"],
  "groupsClaim": "groups",
  "displayName": "Company SSO"
}
```

The provider is discovered at `<issuer>/.well-known/openid-configuration`; register the
redirect URI `<externalURL>/.auth/openidconnect/callback`. `configID` sets a stable identifier
when several OIDC providers are configured. `allowSignup` and
[`linkByEmail`](#linking-by-email) work as for GitLab.

### HTTP header (authenticating proxy)

```jsonc
{
  "type": "http-header",
  "usernameHeader": "X-Forwarded-User",
  "emailHeader": "X-Forwarded-Email",
  "stripUsernameHeaderPrefix": "accounts.example.com:",
  "trustedProxies": ["10.0.0.0/8"]
}
```

For deployments behind a proxy that authenticates users (for example oauth2-proxy). Headers are
accepted only from addresses in `trustedProxies`; an empty list trusts nobody. Users are
created on first request.

### Other access controls

- `auth.public: true` lets signed-out visitors search the repositories that are visible to
  everyone.
- `auth.allowedIpAddress` restricts access to IP addresses or CIDR ranges (with trusted proxy
  and client IP header settings for deployments behind a load balancer).

SAML sign-in is not supported; use OpenID Connect, which most SAML identity providers also offer.
Saving a site configuration with a `saml` provider fails with "SAML sign-in is not supported", and
so does `sourcegull config check`. A stored configuration that already has one still loads: the
`saml` entry is left out and an error is logged, and the rest of the configuration applies.

## Repository permissions

By default, every user can read every repository. A GitLab code host connection with an
`authorization` block makes its private and internal projects readable only by the Sourcegull users
who can read them on GitLab:

```jsonc
// in the code host connection
"authorization": { "identityProvider": { "type": "oauth" } }
```

| `identityProvider.type` | How Sourcegull users are matched to GitLab users | Token |
|---|---|---|
| `oauth` | users who signed in with the [GitLab OAuth provider](#gitlab-oauth) for the same GitLab URL | each user's own OAuth token |
| `username` | same username on GitLab and Sourcegull | the connection token needs admin rights (`sudo`) |

Permissions are synced in the background, both per user (which projects can this user read?)
and per repository (who can read this project?), and again on GitLab membership webhook
events. Public projects are readable by everyone; `markInternalReposAsPublic` treats internal
projects the same way. Site admins can see each user's and repository's permissions and sync
history, and trigger a sync, under Site admin → Repository permissions and on the user and
repository pages. The `permissions.sync*` keys tune the schedule.

Every sign-in queues a sync of that user ahead of other syncs. Until it finishes, a user who
signs in for the first time sees only the repositories everyone can read, so Sourcegull shows a
banner on every page while it runs ("Syncing your repository permissions from your code host"),
then offers to reload the page.

**Gitea** connections take the same `authorization` block. Private and internal repositories
(internal: marked internal, or owned by a limited organization or user) become readable only by
the Sourcegull users who can read their code on Gitea:

| `identityProvider.type` | How Sourcegull users are matched to Gitea users | Token |
|---|---|---|
| `oauth` | users who signed in with the [Gitea OAuth provider](#gitea-oauth) for the same Gitea URL | each user's own OAuth token (scope `read:repository`) |
| `username` | same username on Gitea and Sourcegull (deactivated Gitea users match no one) | the connection token belongs to a Gitea site administrator, which acts as each user with `Sudo` |

Per user, the sync lists the repositories the user owns, collaborates on or reaches through an
organization team, and keeps those whose code they can read. With `username`, each repository is
also synced on its own: its collaborators, the members of the organization teams with code
access to it (the `Owners` team included), or the owner of a repository owned by a user.
Internal repositories that a user reaches only by being signed in to Gitea (not as an owner,
collaborator or team member) are not granted. Gitea sends no webhooks for membership or
visibility changes, so these apply at the next scheduled sync (`permissions.sync*`); a webhook
for a created, deleted or forked repository syncs the connection, and with `username` the new
repository's permissions.

### GitHub repository permissions

A GitHub connection (github.com or GitHub Enterprise Server) with the same `authorization` block
makes its private repositories, and internal ones unless `markInternalReposAsPublic` is set,
readable only by the Sourcegull users who can read them on GitHub. Only `identityProvider: oauth` is
supported: users sign in with the [GitHub OAuth provider](#github-oauth) for the same URL, and
their own tokens list the repositories they own, collaborate on or reach through an
organization. GitHub cannot act as another user, so `username` is rejected.

- Sign-in asks for the `repo` scope while such a connection exists. Users who signed in before
  it was added have tokens without it; their syncs fail with "sign in again to grant access to
  private repositories" and grant nothing until they sign in again.
- Per repository, Sourcegull lists the repository's collaborators (direct, through teams and
  through the organization's base permission) with the connection's credentials: a token with
  admin access to the repository, or a [GitHub App](install.md#github-app-authentication)
  installation with the repository **Metadata** and organization **Members** read permissions.
  Without them the repository sync fails with an error saying so, and the per-user syncs still
  apply. Collaborators are matched only to users who signed in with
  GitHub.
- A revoked token fails the user's sync, removes their synced grants and marks the account as
  needing sign-in.
- Webhooks (the connection's `webhooks` secrets) apply changes right away: `member`,
  `membership` and `organization` (member added or removed) events sync that user;
  `team_add` and `repository` (made private or public, transferred) events sync that
  repository. Subscribe the organization webhook to those events.

**Signing in again**: when a user's GitHub or Gitea token lacks the scope their sync needs, or
a code host rejected their token (GitLab, GitHub or Gitea), Sourcegull shows them a banner on every
page: "Sign in again with GitHub to see your private repositories", linking to that code host's
sign-in provider. It goes away once they sign in again. The sync jobs under Site admin →
Repository permissions show each code host's error with its name.

**Explicit permissions**: with `"permissions.userMapping": { "enabled": true, "bindID":
"email" }` (or `"username"`), every private repository is restricted and site admins grant
access through the GraphQL API, naming users by email or username.

Permissions are enforced on every read path (search, file views, navigation, trends,
watches, notebooks, bulk changes, MCP, …): a repository you cannot read looks like one that
does not exist. Site admins read every repository unless `authz.enforceForSiteAdmins` is set.
Grants and revocations apply immediately.

## Roles (RBAC)

Every user has the `USER` role; site admins also have `SITE_ADMINISTRATOR`. Site admins can
create more roles under **Site admin → Roles** and assign them to users. Permissions that roles
grant include `BATCH_CHANGES#READ`, `BATCH_CHANGES#WRITE`, `OWNERSHIP#ASSIGN` and
`REPO_METADATA#WRITE` ([repository metadata](features.md#repository-metadata)). By default
`USER` grants every permission except `OWNERSHIP#ASSIGN`, and `SITE_ADMINISTRATOR` grants all.

## Organizations and teams

- **Organizations** group users. Members share the organization's settings (part of the
  [settings cascade](configuration.md#settings-and-the-settings-cascade)), saved searches,
  search contexts, notebooks, watches, trends dashboards and bulk changes. Members invite
  others by username or email.
- **Teams** (nested, with members) are used for [code ownership](features.md#code-ownership).
  They can be synced hourly from GitLab groups with `own.syncTeamsFromGitLabGroups`.

## Access tokens

Personal access tokens authenticate API clients, `gull`, CI uploads and [MCP](mcp.md) clients.
Create them under **User settings → Access tokens** (or inline on the Connect an agent page).

```sh
curl -H "Authorization: token <access token>" https://sourcegull.example.com/.api/graphql \
  -d '{"query":"{ currentUser { username } }"}'
```

- Scope `user:all` acts as the token's user. Scope `site-admin:sudo` (site admins only) lets a
  token act as any user with `Authorization: token-sudo user="<username>",token="<token>"`.
- `auth.accessTokens` controls who may create tokens (`all`, `site-admin-create`, `none`) and
  their maximum lifetime (`maxTokenExpiresInDays`, default 365).
- Tokens are stored hashed; a token is shown once, when it is created.

MCP clients can also sign in through the browser instead of using a pasted token; the tokens
they get work only at the MCP endpoint and are listed under **User settings → Connected apps**
([OAuth sign-in](mcp.md#oauth-sign-in)).

## Access requests

When built-in sign-up is off, the sign-in page lets visitors request an account (turn this off
with `"auth.accessRequest": { "enabled": false }`). Site admins review requests under **Site
admin → Access requests**; approving one creates the user and returns a one-time account setup
link, which is also emailed when SMTP is configured.

## Accounts

Users manage their own account under **User settings**:

- **Profile**: display name and avatar URL (an `http` or `https` URL). Users change their own
  username only when `"auth.enableUsernameChanges": true`; site admins can always rename users.
- **Password** (built-in sign-in only): changing it needs the current password, if the account
  has one, and signs the user out of every other session.
- **Emails**: add addresses (a verification link is emailed when `email.smtp` is configured),
  resend verification, make a verified address primary, and remove addresses. The primary and
  the only address cannot be removed.

Site admins can change any user's profile and emails, and deactivate users under **Site admin →
Users**. Deactivation ends the user's sessions, revokes their access tokens and connected apps,
and blocks sign-in; it frees the username. **Show deactivated users** lists them with a
**Reactivate** action, which restores the account (access tokens stay revoked) unless another
user has taken the username meanwhile. Site admins cannot deactivate themselves.

The username, emails and active state of [SCIM](#scim-provisioning)-managed users change only
through the identity provider.

The GraphQL mutations are `updateUser`, `updatePassword`, `addUserEmail`, `removeUserEmail`,
`setUserEmailPrimary`, `resendVerificationEmail`, `deactivateUser` and `reactivateUser`.

## SCIM provisioning

Sourcegull accepts SCIM 2.0 user provisioning from identity providers such as Okta, Microsoft Entra
ID or Keycloak at `<externalURL>/.api/scim/v2`.

1. Set `"scim.authToken": "<random secret of at least 20 characters>"` in the site
   configuration.
2. In the identity provider, configure the SCIM base URL and the token as a Bearer token.

Users can be created, updated, deactivated (`active: false` ends their sessions and revokes their
tokens and connected apps) and deleted, with filtering and pagination. Groups are not provisioned.

## Audit events

Security events (sign-ins and failed sign-ins, sign-outs, password changes, access token
creation and use, OAuth grants, refreshes and revocations of MCP clients, role and permission
changes, site configuration changes and other admin actions) are recorded according to `log.securityEventLog.location`: `database` (the default),
`logs` (structured application logs) or `all`.
