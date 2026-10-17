#!/usr/bin/env bash
# Production test suite. Quick mode (default) takes ~3 minutes; FULL=1 runs longer soaks
# and more crash cycles.
set -uo pipefail
cd "$(dirname "$0")/../.."
cargo build --release -q || exit 1
cargo test --release -q 2>&1 | grep -E "test result|FAILED|panicked"
FULL=${FULL:-0}
export CYCLES=$([ "$FULL" = 1 ] && echo 40 || echo 10)
export SOAK_SECONDS=$([ "$FULL" = 1 ] && echo 900 || echo 60)
export CASES=$([ "$FULL" = 1 ] && echo 20000 || echo 3000)
failed=0
for t in differential regex_diff robustness crash soak pitr cluster2 nlq; do
  python3 "tests/prod/$t.py" 2>&1 | grep -E "^(===|---|  FAIL|  cycle|  t=|    |  memory|  fuzzing)" || true
  [ "${PIPESTATUS[0]}" = 0 ] || failed=$((failed + 1))
done
# GeoSPARQL geometry vs GEOS (needs uv to fetch shapely into a throwaway environment)
if command -v uv >/dev/null; then
  uv run -q --with shapely python3 tests/prod/geo_diff.py "$CASES" 2>&1 | tail -1
  [ "${PIPESTATUS[0]}" = 0 ] || failed=$((failed + 1))
fi
bash bench/cluster-test.sh 2>&1 | grep -E "PASS|FAIL" || failed=$((failed + 1))
# against real services in Docker (DOCKER=1): OpenSearch, Keycloak, OpenLDAP
if [ "${DOCKER:-0}" = 1 ] && command -v docker >/dev/null; then
  for t in opensearch keycloak ldap; do
    python3 "tests/prod/$t.py" 2>&1 | grep -E "^(===|---|  FAIL)" || true
    [ "${PIPESTATUS[0]}" = 0 ] || failed=$((failed + 1))
  done
fi
echo; [ $failed = 0 ] && echo "ALL PRODUCTION TESTS PASSED" || { echo "$failed SUITE(S) FAILED"; exit 1; }
