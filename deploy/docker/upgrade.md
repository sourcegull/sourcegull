# Upgrading Sourcegull on Docker Compose

Sourcegull migrations follow expand → migrate → contract: the schema of
version N+1 stays compatible with version N, so you can upgrade one minor version at a time with
a short restart. To skip versions, stop Sourcegull and run a multi-version upgrade with the new
image (see [Upgrading across several versions](#upgrading-across-several-versions)).

## Before you upgrade

1. Read the release notes for the target version.
2. Back up the database and the encryption key file:

   ```sh
   cd deploy/docker
   docker compose exec -T postgres pg_dump -U sourcegull -Fc sourcegull > sourcegull-$(date +%F).dump
   cp secrets/encryption-keys secrets/encryption-keys.bak
   ```

## Upgrade: pull → migrate → up

```sh
cd deploy/docker

# 1. Point the stack at the new version.
sed -i 's/^SOURCEGULL_VERSION=.*/SOURCEGULL_VERSION=0.2.0/' .env

# 2. Pull the new images.
docker compose pull

# 3. Run the migrations with the new image (one-shot; exits non-zero on failure).
docker compose run --rm migrate

# 4. Recreate the services on the new image.
docker compose up -d

# 5. Check that everything is healthy.
docker compose ps
docker compose run --rm migrate migrate status
```

`docker compose up -d` alone also works: `web` and `worker` wait for the `migrate` one-shot to
finish successfully (`service_completed_successfully`). Step 3 just surfaces migration errors
before any running service is replaced.

## Upgrading across several versions

The newest image carries every migration, so it can upgrade any older version in one run. It
checks that the database is exactly at `--from`, compares the schema with the one that version
shipped (drift), applies each version's migrations in order and checks the result:

```sh
cd deploy/docker
docker compose stop web worker repo-0
sed -i 's/^SOURCEGULL_VERSION=.*/SOURCEGULL_VERSION=0.4.0/' .env
docker compose pull
docker compose run --rm migrate migrate versions                    # where the database is
docker compose run --rm migrate migrate upgrade --from 0.2.0 --dry-run   # the plan
docker compose run --rm migrate migrate upgrade --from 0.2.0 --to 0.4.0
docker compose up -d
```

This flow (with a key rotation in the same window) is tested from two releases back:
`scripts/test/upgrade.sh compose` (`.github/workflows/upgrade.yml`).

If the drift check finds differences (manual schema changes, a migration repaired by hand), it
prints each one with SQL that would fix it and changes nothing. Review and apply the fixes (or
rerun with `--skip-drift-check` if you know the differences are harmless). Check a running
installation at any time with `docker compose run --rm migrate migrate drift`; it exits 1 on
drift.

## Out-of-band migrations

Some data migrations run in the background after an upgrade (workers run them in batches; the
site admin **Migrations** page and `docker compose run --rm migrate migrate status` show their
progress). A release that deprecates such a migration no longer reads the old data format, so
`migrate upgrade` and the services refuse to move to it until the migration reaches 100%: upgrade
to the release before it first, start it and wait. The site admin **Updates** page lists the
migrations that must finish before the next upgrade.

## Rotating the encryption key

Secrets (code host configs, tokens, credentials, executor secrets, webhook secrets and URLs,
watch webhook URLs) are encrypted
with data keys that are wrapped by the key-encryption keys in `secrets/encryption-keys`.
To rotate:

1. Append a new key as the **last** line (the last line is the current key; keep the old ones):

   ```sh
   echo "k2:$(openssl rand -base64 32)" >> secrets/encryption-keys
   ```

2. Restart the services (`docker compose up -d --force-recreate`). The first process to start
   with the new key re-wraps every data key with it, creates new data keys and retires the old
   ones (decrypt-only); the others reload the keys.
3. The workers re-encrypt every secret with the new data keys (out-of-band migration 1, "Re-encrypt
   secrets"). When the Migrations page shows it at 100%, the old key lines can be removed.

Watch webhook URLs saved before keys were configured (or before this release) stay in
plaintext until out-of-band migration 2 ("Encrypt watch webhook URLs") encrypts them; it
runs on its own once keys exist. Before downgrading to a release without it, apply it in reverse
on the Migrations page so the URLs are plaintext again.

`sourcegull keys status` lists the data keys and how many secrets are still on retired keys;
`sourcegull keys rotate` creates new data keys without changing the key file (same re-encryption).

## If a migration fails

The old containers keep running on the old version (the schema change is additive). Fix the cause
shown in the `migrate` logs and run step 3 again. To go back, restore the dump taken above:

```sh
docker compose stop web worker
docker compose exec -T postgres pg_restore -U sourcegull -d sourcegull --clean --if-exists < sourcegull-YYYY-MM-DD.dump
sed -i 's/^SOURCEGULL_VERSION=.*/SOURCEGULL_VERSION=<previous>/' .env
docker compose up -d
```

## Upgrading PostgreSQL

The `postgres` service follows one major version (`postgres:18-bookworm`); `docker compose pull`
brings only its minor updates. A new major version cannot start on the old one's data directory,
so a major upgrade is a dump and restore.

```sh
cd deploy/docker
docker compose stop web worker repo-0
docker compose exec -T postgres pg_dump -U sourcegull -Fc sourcegull > sourcegull-pg.dump
docker compose down
docker volume rm sourcegull_postgres-data   # the dump is now the only copy: keep it
# Change the postgres image in compose.yaml to the new major version, then:
docker compose up -d --wait postgres
docker compose exec -T postgres pg_restore -U sourcegull -d sourcegull --no-owner < sourcegull-pg.dump
docker compose up -d
```

For a large database, `pg_upgrade` with both versions' binaries is faster than a dump and
restore.
