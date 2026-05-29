#!/usr/bin/env bash
set -euo pipefail

PROXY="${PROXY:-http://127.0.0.1:3030}"

echo "Using PROXY=$PROXY"
echo

check() {
  local name="$1"
  shift
  echo "===== $name ====="
  echo "curl $*"
  echo
  # -s silent, -D - print headers, -o /dev/null ignore body
  curl -s -D - -o /dev/null "$@"
  echo
  echo
}

# 1) OPTIONS preflight (respond_preflight)
check "OPTIONS /observer/notify-bridgeout" \
  -X OPTIONS "${PROXY}/observer/notify-bridgeout" \
  -H "Origin: http://localhost:8080" \
  -H "Access-Control-Request-Method: POST" \
  -H "Access-Control-Request-Headers: content-type"

# 2) POST notify-bridgeout, valid body (200 accepted via respond_json)
check "POST /observer/notify-bridgeout (valid body)" \
  -X POST "${PROXY}/observer/notify-bridgeout" \
  -H "Content-Type: application/json" \
  -d '{"chainId":80002}'

# 3) POST notify-bridgeout, invalid body (400 via respond_json)
check "POST /observer/notify-bridgeout (invalid body)" \
  -X POST "${PROXY}/observer/notify-bridgeout" \
  -H "Content-Type: application/json" \
  -d '{}'

# 4) GET transaction (happy path; 200 or 503/504 depending on observer_urls)
check "GET /observer/transaction?page=1" \
  "${PROXY}/observer/transaction?page=1"

# 5) GET transaction with bad query (400 via respond_json)
check "GET /observer/transaction?page=0 (bad query)" \
  "${PROXY}/observer/transaction?page=0"

# 6) Unsupported observer route (404 via respond_json)
check "GET /observer/unknown (404)" \
  "${PROXY}/observer/unknown"

# 7) Wrong method on notify-bridgeout (405 via respond_json)
check "GET /observer/notify-bridgeout (405)" \
  "${PROXY}/observer/notify-bridgeout"

echo "Done. Verify all above have:"
echo "  Access-Control-Allow-Methods: GET, POST, OPTIONS"
echo "and appropriate status codes for each case."