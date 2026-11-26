---
id: hold-lease
title: Hold a path lease
when: Before you start editing paths other agents may also change. Take, renew and release a lease, and know what to do when someone else holds one.
---
You are going to coordinate work in {{repo}} on {{endpoint}} with path leases. A lease announces that you are about to change some paths on a ref, so others can route around you. It does not lock git history; it is a signal that policy can also enforce. If you have not connected yet, follow the "Set up Bee for a repository" prompt first. Your token needs the `lease` action.

1. Look before you take. List the live leases that overlap your paths:

   ```sh
   bee lease list-leases --repo {{repo}} --paths src/billing --json
   ```

   Each lease has a holder, paths, mode (`advisory` or `exclusive`), a note and an expiry.

2. Take a lease.

   ```sh
   bee lease acquire-lease --repo {{repo}} --ref main --paths src/billing --paths docs/billing.md \
     --mode exclusive --ttl-seconds 3600 --note "Fix rounding in invoices" --json
   ```

   The default mode is `advisory`; the default lifetime is 2 hours and the longest is 24 hours (`ttl_too_long` otherwise). Ask for what you need: a long lease starves others. Take an `exclusive` lease when your change would break if someone else edited the same files; use `advisory` to announce yourself. An advisory lease always succeeds and returns `overlapping`, the advisory leases of others on those paths; read them and message the holders or split the work. Keep `data.lease.id`.

3. Renew before it expires. A lease that expired is gone, and renewing it answers `NOT_FOUND`.

   ```sh
   bee lease renew-lease --repo {{repo}} --id <lease id> --ttl-seconds 3600 --json
   ```

   Renewing sets the expiry to now plus the lifetime you pass. Only the holder renews: anyone else gets `PERMISSION_DENIED`.

4. Release the moment you are done, including when you abandon the work:

   ```sh
   bee lease release-lease --repo {{repo}} --id <lease id>
   ```

   A lease tied to a change (`--change-id` when you acquire it) is released when that change lands or its change request closes. An expired or already released lease answers `NOT_FOUND`; that is not a problem.

5. When someone else holds an exclusive lease on your paths, `acquire-lease` fails with the reason `LEASED` (the CLI exits with code 5). The error payload `lease_conflict` lists the conflicting lease ids and paths. Do not edit those paths and do not try to land a change that touches them: a land through an exclusive lease of another principal fails with `LEASED` too. Choose one:
   - Do other work first. Check again with `list-leases` after a pause; leases expire and are released when their change lands.
   - Narrow your paths to avoid the overlap, and take a new lease on the smaller set.
   - Ask the holder (the lease shows who) to release, or ask a maintainer, who can release a lease on your behalf.
   - Wait for the lease to end, and watch for the event `lease.released` instead of polling.

Over MCP the tools are `lease_acquire_lease`, `lease_renew_lease`, `lease_release_lease`, `lease_list_leases` and `lease_get_lease`. Report the lease id, its paths and mode, its expiry, and any overlapping leases you found.
