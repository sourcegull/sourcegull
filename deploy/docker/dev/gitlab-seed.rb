# Seeds the development GitLab (compose.yaml, service `gitlab`) with fixed data. Idempotent.
# Run by `scripts/dev gitlab up` as `gitlab-rails runner /seed/gitlab-seed.rb`.
#
# Everything here is a fixed, development-only value; scripts/dev and the live tests in
# crates/codehost-gitlab/tests/live.rs rely on them, so change them together.
#
#   root                admin; password Xq7v-Lm2k-Pz9w; token glpat-sourcegull-dev-root (api, sudo)
#   alice, bob, carol   password Hn4t-Rb8c-Wy3j; tokens glpat-sourcegull-dev-<name> (read_api)
#   OAuth application   client sourcegull-dev / secret sourcegull-dev-secret, redirect URIs on :7080, :5173
#
# Groups: acme (internal), acme/platform (private), community (public).
# Projects (all with a README commit unless noted):
#   acme/api            private   alice developer, bob reporter
#   acme/web            private   alice developer
#   acme/empty          private   no commits
#   acme/legacy         private   archived
#   acme/handbook       internal
#   acme/platform/tools private   (subgroup) carol guest (cannot read code)
#   community/docs      public
# alice is also a member (developer) of the group acme/platform.

$stdout.sync = true

USER_PASSWORD = 'Hn4t-Rb8c-Wy3j'
EXPIRES = Date.today + 360

def say(msg)
  puts "seed: #{msg}"
end

def check!(result, what)
  return result unless result.respond_to?(:success?)
  raise "#{what}: #{result.message}" unless result.success?
  result
end

def org
  Organizations::Organization.default_organization
end

def token!(user, name, value, scopes)
  existing = user.personal_access_tokens.find_by(name: name)
  return if existing && existing.active? && existing.scopes.map(&:to_s).sort == scopes.map(&:to_s).sort
  existing&.destroy!
  t = user.personal_access_tokens.build(name: name, scopes: scopes, expires_at: EXPIRES)
  t.organization = org if t.respond_to?(:organization=)
  t.set_token(value)
  t.save!
  say "token #{name} for #{user.username}"
end

root = User.find_by_username!('root')

settings = ApplicationSetting.current
settings.update!(
  signup_enabled: false,
  allow_local_requests_from_web_hooks_and_services: true,
  allow_local_requests_from_system_hooks: true,
)
token!(root, 'sourcegull-dev', 'glpat-sourcegull-dev-root', %i[api read_api sudo admin_mode])

users = %w[alice bob carol].to_h do |name|
  user = User.find_by_username(name)
  unless user
    result = Users::CreateService.new(root,
      username: name,
      name: name.capitalize,
      email: "#{name}@sourcegull.localhost",
      password: USER_PASSWORD,
      password_confirmation: USER_PASSWORD,
      skip_confirmation: true,
      organization_id: org.id).execute
    check!(result, "create user #{name}")
    user = User.find_by_username!(name)
    say "user #{name}"
  end
  token!(user, 'sourcegull-dev', "glpat-sourcegull-dev-#{name}", %i[read_api read_user])
  [name, user]
end

def group!(root, path, parent: nil, visibility: Gitlab::VisibilityLevel::PRIVATE)
  full = parent ? "#{parent.full_path}/#{path}" : path
  group = Group.find_by_full_path(full)
  if group
    group.update!(visibility_level: visibility) if group.visibility_level != visibility
    return group
  end
  params = { name: path.capitalize, path: path, visibility_level: visibility, organization_id: org.id }
  params[:parent_id] = parent.id if parent
  result = Groups::CreateService.new(root, params).execute
  check!(result, "create group #{full}")
  say "group #{full}"
  Group.find_by_full_path(full) || raise("group #{full} not found after create")
end

def project!(root, group, path, visibility: Gitlab::VisibilityLevel::PRIVATE, readme: true)
  full = "#{group.full_path}/#{path}"
  project = Project.find_by_full_path(full)
  if project
    project.update!(visibility_level: visibility) if project.visibility_level != visibility
    return project
  end
  project = Projects::CreateService.new(root,
    name: path,
    path: path,
    namespace_id: group.id,
    visibility_level: visibility,
    initialize_with_readme: readme,
    organization_id: org.id).execute
  raise "create project #{full}: #{project.errors.full_messages.join(', ')}" unless project.persisted?
  say "project #{full}"
  project
end

# Internal, so that acme/handbook can be internal (a project is at most as visible as its group).
acme = group!(root, 'acme', visibility: Gitlab::VisibilityLevel::INTERNAL)
platform = group!(root, 'platform', parent: acme)
community = group!(root, 'community', visibility: Gitlab::VisibilityLevel::PUBLIC)

api = project!(root, acme, 'api')
web = project!(root, acme, 'web')
project!(root, acme, 'empty', readme: false)
legacy = project!(root, acme, 'legacy')
project!(root, acme, 'handbook', visibility: Gitlab::VisibilityLevel::INTERNAL)
tools = project!(root, platform, 'tools')
project!(root, community, 'docs', visibility: Gitlab::VisibilityLevel::PUBLIC)

unless legacy.archived?
  check!(Projects::UpdateService.new(legacy, root, archived: true).execute, 'archive acme/legacy')
  say 'archived acme/legacy'
end

api.add_member(users['alice'], :developer)
api.add_member(users['bob'], :reporter)
web.add_member(users['alice'], :developer)
tools.add_member(users['carol'], :guest)
platform.add_member(users['alice'], :developer)

app = Doorkeeper::Application.find_by(uid: 'sourcegull-dev') || Doorkeeper::Application.new(uid: 'sourcegull-dev')
app.assign_attributes(
  name: 'Sourcegull (development)',
  redirect_uri: "http://localhost:7080/.auth/gitlab/callback\nhttp://localhost:5173/.auth/gitlab/callback",
  scopes: 'read_user read_api',
  confidential: true,
)
app.organization_id = org.id
# Stored hashed, as GitLab does for applications created in the UI.
Doorkeeper.config.application_secret_strategy.store_secret(app, :secret, 'sourcegull-dev-secret')
app.save!
say 'OAuth application sourcegull-dev'

say 'done'
