# Optional JavaScript framework integrations

`@hyphae_/hyphae-integrations` has independent subpath exports for Astro,
Next, and Vite. `astro`, `next`, and `vite` are optional peer dependencies; an
application installs only the host and integration it chooses.

The `3.0.0` package is maintained as source-only in this repository. Per the
`3.0.0` release receipt's distribution boundary, it is not published to npm.

- `./astro` creates middleware that places a public client in request-local
  `Astro.locals`.
- `./next` constructs a server-only client from explicit options or private
  runtime `HYPHAE_*` environment values. Never use `NEXT_PUBLIC_` for secrets.
- `./vite` configures a secret-free `/v1` development/preview proxy.
- `./vite/client` creates a browser client against a same-origin proxy. The
  production server/reverse proxy remains application-owned.

No adapter starts Hyphae, opens its data directory, or imports core internals.
Removing this package leaves every host framework unchanged.
