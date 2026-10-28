-- The OpenID Connect nonce of a sign-in flow (§6.22); NULL for GitLab OAuth flows.
ALTER TABLE oauth_flows ADD COLUMN nonce text;
