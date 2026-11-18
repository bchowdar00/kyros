#!/bin/sh
set -eu

# High-confidence markers only. Split literals keep this scanner from matching
# its own source; tracked fixtures must never contain real private keys/tokens.
aws_prefix='AKI''A[0-9A-Z]{16}'
github_token='gh''[ps]_[A-Za-z0-9]{30,}'
private_key='-----BEGIN (RSA |EC |OPENSSH )?PRIVATE KEY-----'

# SportsDataIO NBA (gate SDIO-1, `signals/sports/nba/SPORTSDATAIO-TRUE-TRIAL-GATE.md`).
# The provider permits either a `?key=` query parameter or the
# `Ocp-Apim-Subscription-Key` header; Tiny uses ONLY the header, so both shapes
# below are markers rather than style preferences.
#
#  1. a `key=` (or `&key=`) query parameter on a SportsDataIO URL. Anchored to the
#     host so prose about `?key=` -- which the auth module and this comment both
#     contain -- is not a match. The parameter name is matched case-INSENSITIVELY:
#     the provider's query mode is not case-sensitive, so `?Key=` is the same
#     leak.
#  2. a 32-hexadecimal-character run on the same line as the subscription-key
#     header name. The header NAME is a public literal that appears legitimately
#     in `sportsdataio_auth.rs`, `capture.rs` and the tests, so it is deliberately
#     NOT a marker on its own; only a key-shaped value beside it is. The
#     SportsDataIO key format is 32 hex characters, and every sentinel in the
#     tests is chosen not to be, so this fires on real key material only. The hex
#     class is case-INSENSITIVE: the header-name match already is, and a key
#     pasted in upper case is the same key.
#  3. a key-shaped value on the same line as the environment-variable NAME. The
#     bare name is a public literal that appears legitimately in the auth module,
#     ADR 0021 and the docs, so -- like the header name -- it is NOT a marker on
#     its own; only a 32-hex value within eight characters of it is. The short
#     gap covers both `SPORTSDATAIO_NBA_API_KEY=<32hex>` and the exported form
#     `export SPORTSDATAIO_NBA_API_KEY="<32hex>"` without reaching across a line
#     of unrelated prose.
sdio_query_key='api\.sportsdata\.io[^[:space:]]*[?&][Kk][Ee][Yy]='
sdio_header_key='[Oo]cp-[Aa]pim-[Ss]ubscription-[Kk]ey.{0,160}[0-9a-fA-F]{32}|[0-9a-fA-F]{32}.{0,160}[Oo]cp-[Aa]pim-[Ss]ubscription-[Kk]ey'
sdio_environment_key='SPORTSDATAIO_NBA_API_KEY.{0,8}[0-9a-fA-F]{32}'

# With arguments, scan exactly those files instead of the tracked tree. The paths
# need not be inside the repository: this is how the SportsDataIO contract test
# proves the markers above actually fire, without ever committing one.
if [ "$#" -gt 0 ]; then
  if grep -n -I -E -e "$aws_prefix|$github_token|$private_key|$sdio_query_key|$sdio_header_key|$sdio_environment_key" -- "$@"; then
    echo "the scanned content contains a credential marker" >&2
    exit 1
  fi
  exit 0
fi

# Tracked-tree mode needs a repository: without one, `git grep` fails and the
# `if git grep ...; then fail` shape below would read that failure as "no
# match" and exit 0 — a silent false pass (seen 2026-09-04 on a plain copy).
if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  echo "check-secrets: not inside a git repository; pass explicit files to scan a plain tree" >&2
  exit 2
fi

if git grep -n -I -E -e "$aws_prefix|$github_token" -- .; then
  echo "tracked repository content contains a credential marker" >&2
  exit 1
fi

# The key decoder's negative tests intentionally contain header text, but no
# key material. Exclude that one audited parser source from the PEM marker scan.
if git grep -n -I -E -e "$private_key" -- . ':!crates/btc-feeds/src/kalshi_auth.rs'; then
  echo "tracked repository content contains a credential marker" >&2
  exit 1
fi

if git grep -n -I -E -e "$sdio_query_key" -e "$sdio_header_key" -e "$sdio_environment_key" -- .; then
  echo "tracked repository content contains a SportsDataIO credential marker" >&2
  exit 1
fi
