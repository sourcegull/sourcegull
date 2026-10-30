# Upgrading Sourcegull on Docker Compose

Sourcegull migrations follow expand → migrate → contract: the schema of
version N+1 stays compatible with version N, so you can upgrade one minor version at a time with
a short restart. To skip versions, stop Sourcegull and run a multi-version upgrade with the new
image (see [Upgrading across several versions](#upgrading-across-several-versions)).

Commands run in the directory of the Compose bundle (`sourcegull-compose`, or `deploy/docker` in a
checkout). They are given for `compose.yaml` (one `sourcegull` container) and, where they differ,
for the production layout (`compose.production.yaml`, set as `COMPOSE_FILE` in `.env`).

## Before you upgrade

1. Read the release notes for the target version.
2. Back up the database and the encryption key file:

   ```sh
   cd sourcegull-compose
   docker compose exec -T postgres pg_dump -U sourcegull -Fc sourcegull > sourcegull-$(date +%F).dump
   docker compose cp sourcegull:/var/opt/sourcegull/keys sourcegull-keys.bak
   # compose.production.yaml: cp secrets/encryption-keys secrets/encryption-keys.bak
   ```

## Upgrade: pull → migrate → up

```sh
cd sourcegull-compose

# 1. Take the new release's Compose files (extracted over the old ones; the bundle holds no
#    .env, secrets/ or files of yours in config/), and point the stack at the new version.
curl -fsSL https://github.com/sourcegull/sourcegull/releases/download/v0.2.0/sourcegull-compose.tar.gz | tar -xz -C ..
sed -i 's/^SOURCEGULL_VERSION=.*/SOURCEGULL_VERSION=0.2.0/' .env

# 2. Pull the new images.
docker compose pull

# 3. Run the migrations with the new image while the old version keeps serving (one-shot;
#    exits non-zero on failure).
docker compose run --rm --no-deps sourcegull migrate up
# compose.production.yaml: docker compose run --rm migrate

# 4. Recreate the services on the new image.
docker compose up -d

# 5. Check that everything is healthy.
docker compose ps
docker compose run --rm --no-deps sourcegull migrate status
# compose.production.yaml: docker compose run --rm migrate migrate status
```

`docker compose up -d` alone also works: `sourcegull supervise` applies the migrations when it
starts, and in the production layout `web` and `worker` wait for the `migrate` one-shot to
finish successfully (`service_completed_successfully`). Step 3 just surfaces migration errors
before any running service is replaced.

## Upgrading across several versions

The newest image carries every migration, so it can upgrade any older version in one run. It
checks that the database is exactly at `--from`, compares the schema with the one that version
shipped (drift), applies each version's migrations in order and checks the result:

```sh
cd sourcegull-compose
docker compose stop sourcegull          # compose.production.yaml: stop web worker repo-0
sed -i 's/^SOURCEGULL_VERSION=.*/SOURCEGULL_VERSION=0.4.0/' .env
docker compose pull
# compose.production.yaml: `run --rm migrate migrate …` in place of `run --rm --no-deps sourcegull migrate …`
docker compose run --rm --no-deps sourcegull migrate versions                    # where the database is
docker compose run --rm --no-deps sourcegull migrate upgrade --from 0.2.0 --dry-run   # the plan
docker compose run --rm --no-deps sourcegull migrate upgrade --from 0.2.0 --to 0.4.0
docker compose up -d
```

This flow (with a key rotation in the same window) is tested from two releases back with the
production layout: `scripts/test/upgrade.sh compose` (`.github/workflows/upgrade.yml`).

If the drift check finds differences (manual schema changes, a migration repaired by hand), it
prints each one with SQL that would fix it and changes nothing. Review and apply the fixes (or
rerun with `--skip-drift-check` if you know the differences are harmless). Check a running
installation at any time with `docker compose run --rm --no-deps sourcegull migrate drift`
(production layout: `docker compose run --rm migrate migrate drift`); it exits 1 on drift.

## Out-of-band migrations

Some data migrations run in the background after an upgrade (workers run them in batches; the
site admin **Migrations** page and `migrate status` (step 5 above) show their progress). A
release that deprecates such a migration no longer reads the old data format, so
`migrate upgrade` and the services refuse to move to it until the migration reaches 100%: upgrade
to the release before it first, start it and wait. The site admin **Updates** page lists the
migrations that must finish before the next upgrade.

## Rotating the encryption key

Secrets (code host configs, tokens, credentials, executor secrets, webhook secrets and URLs,
watch webhook URLs) are encrypted
with data keys that are wrapped by the key-encryption keys in the key file
(`/var/opt/sourcegull/keys` in the `sourcegull-data` volume; `secrets/encryption-keys` in the
production layout). To rotate:

1. Append a new key as the **last** line (the last line is the current key; keep the old one):

   ```sh
   docker compose exec sourcegull bash -c 'echo "k2:$(head -c 32 /dev/urandom | base64)" >> /var/opt/sourcegull/keys'
   # compose.production.yaml:
   echo "k2:$(openssl rand -base64 32)" >> secrets/encryption-keys
   ```

2. Restart the services (`docker compose up -d --force-recreate`). Changing the file alone does
   not rotate. The first process to start with the new key re-wraps every data key with it,
   creates new data keys and retires the old ones (decrypt-only); the others reload the keys.
3. The workers re-encrypt every secret with the new data keys (out-of-band migration 1, "Re-encrypt
   secrets"). When the Migrations page shows it at 100%, delete the old key line; no restart is
   needed. The file is then back to one line: since the data keys were re-wrapped, the old key
   is never needed again, and a rotation never needs more than two lines.

Watch webhook URLs saved before keys were configured (or before this release) stay in
plaintext until out-of-band migration 2 ("Encrypt watch webhook URLs") encrypts them; it
runs on its own once keys exist. Before downgrading to a release without it, apply it in reverse
on the Migrations page so the URLs are plaintext again.

`sourcegull keys status` lists the data keys and how many secrets are still on retired keys;
`sourcegull keys rotate` creates new data keys without changing the key file (same re-encryption).

## If a migration fails

The old containers keep running on the old version (the schema change is additive). Fix the cause
shown in the output of step 3 and run it again. If `sourcegull supervise` fails to migrate on
start, it exits and Compose restarts it until the migration succeeds; `docker compose logs
sourcegull` shows the error. To go back, restore the dump taken above:

```sh
docker compose stop sourcegull          # compose.production.yaml: stop web worker
docker compose exec -T postgres pg_restore -U sourcegull -d sourcegull --clean --if-exists < sourcegull-YYYY-MM-DD.dump
sed -i 's/^SOURCEGULL_VERSION=.*/SOURCEGULL_VERSION=<previous>/' .env
docker compose up -d
```

## Upgrading PostgreSQL

The `postgres` service follows one major version (`postgres:18-bookworm`); `docker compose pull`
brings only its minor updates. A new major version cannot start on the old one's data directory,
so a major upgrade is a dump and restore.

```sh
cd sourcegull-compose
docker compose stop sourcegull          # compose.production.yaml: stop web worker repo-0
docker compose exec -T postgres pg_dump -U sourcegull -Fc sourcegull > sourcegull-pg.dump
docker compose down
docker volume rm sourcegull_postgres-data   # the dump is now the only copy: keep it
# Change the postgres image in the Compose file to the new major version, then:
docker compose up -d --wait postgres
docker compose exec -T postgres pg_restore -U sourcegull -d sourcegull --no-owner < sourcegull-pg.dump
docker compose up -d
```

For a large database, `pg_upgrade` with both versions' binaries is faster than a dump and
restore.
