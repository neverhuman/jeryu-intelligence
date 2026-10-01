#!/usr/bin/env bash
# Fail-closed selection tests for the release broker's governed Jankurai path.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${here}/../.." && pwd)"
source_lib="${here}/lib.sh"
source_verifier="${here}/ensure-jankurai.sh"
production_broker="/opt/jain-ci/authority/release-bin/jankurai"
tmp="$(mktemp -d /tmp/test-governed-jankurai-path.XXXXXX)"
cleanup() {
  rm -rf -- "${tmp}"
}
trap cleanup EXIT

fail() {
  printf 'test-governed-jankurai-path: %s\n' "$*" >&2
  exit 1
}

expect_failure() {
  local description="$1" pattern="$2"
  shift 2
  if "$@" >"${tmp}/failure.log" 2>&1; then
    fail "${description}: command unexpectedly succeeded"
  fi
  grep -Fq "${pattern}" "${tmp}/failure.log" || {
    sed -n '1,80p' "${tmp}/failure.log" >&2
    fail "${description}: expected failure text was absent"
  }
}

for source in \
  "${source_lib}" \
  "${source_verifier}"; do
  grep -Fq "${production_broker}" "${source}" ||
    fail "release broker path contract is absent from ${source#"${repo_root}/"}"
  grep -Fq 'local mode=receipt-bound' "${source}" ||
    fail "release broker mode is absent from ${source#"${repo_root}/"}"
  grep -Fq 'local expected_governed="/home/ubuntu/.jeryu/bin/jankurai"' \
    "${source}" ||
    fail "ordinary governed-home selection is absent from ${source#"${repo_root}/"}"
done
for entrypoint in \
  "${here}/pr-ci.sh" \
  "${repo_root}/scripts/ci-doctor.sh"; do
  if grep -Eq '/home/ubuntu/\.jeryu/bin/jankurai|JERYU_JANKURAI_BIN:-' \
    "${entrypoint}"; then
    fail "${entrypoint#"${repo_root}/"} still selects an ambient-home or caller-provided auditor"
  fi
done

mkdir -p "${tmp}/broker/bin" "${tmp}/attacker/bin" \
  "${tmp}/home/.jeryu/bin" "${tmp}/home/.jeryu/receipts/jankurai/sha256" \
  "${tmp}/home/.local/bin"
# The fixture is this host's own verified installation. require_jankurai checks the
# installed binary, its receipt and the host authority stamp, so the test carries no pin.
# The child shell expands its positional inputs.
# shellcheck disable=SC2016
host_identity="$(env -i HOME=/home/ubuntu PATH=/usr/bin:/bin bash -c \
  'source "$1" && require_jankurai >/dev/null && printf "%s\n%s\n" "$JERYU_GOVERNED_JANKURAI_BIN" "$JERYU_JANKURAI_RECEIPT"' \
  bash "${source_lib}")" || fail "host governed Jankurai does not verify"
governed_source="$(sed -n 1p <<<"${host_identity}")"
host_receipt="$(sed -n 2p <<<"${host_identity}")"
[[ "${governed_source}" == /* && -f "${governed_source}" && -f "${host_receipt}" ]] ||
  fail "governed Jankurai test source is unavailable"
expected_version="$("${governed_source}" --version)"
expected_sha="$(sha256sum "${governed_source}" | awk '{print $1}')"

broker_bin="${tmp}/broker/bin/jankurai"
attacker_bin="${tmp}/attacker/bin/jankurai"
ambient_bin="${tmp}/home/.jeryu/bin/jankurai"
older_local_bin="${tmp}/home/.local/bin/jankurai"
cp -- "${governed_source}" "${broker_bin}"
cp -- "${governed_source}" "${attacker_bin}"
cp -- "${governed_source}" "${ambient_bin}"
chmod 0555 "${broker_bin}" "${attacker_bin}" "${ambient_bin}"
printf '#!/usr/bin/env bash\nprintf "%s\\n"\n' "${expected_version}" >"${older_local_bin}"
chmod 0755 "${older_local_bin}"
# A release broker carries no receipt: its digest record sits beside it with
# read-only, single-link custody.
printf '%s\n' "${expected_sha}" >"${broker_bin}.sha256"
chmod 0444 "${broker_bin}.sha256"
# The host authority stamp names the digest the installer last made current.
mkdir -p "${tmp}/home/.jeryu/authority"
jq -n --arg sha "${expected_sha}" --arg version "${expected_version}" \
  '{schema:"jeryu.jankurai-authority-stamp/v1",binary_sha256:$sha,version:$version}' \
  >"${tmp}/home/.jeryu/authority/jankurai.json"

receipt_stage="${tmp}/local-receipt.json"
jq --arg path "${ambient_bin}" '.installation.path = $path' "${host_receipt}" >"${receipt_stage}"
receipt_sha="$(sha256sum "${receipt_stage}")"
receipt_sha="${receipt_sha%% *}"
local_receipt="${tmp}/home/.jeryu/receipts/jankurai/sha256/${receipt_sha}.json"
mv -- "${receipt_stage}" "${local_receipt}"
caller_receipt_stage="${tmp}/caller-local-receipt.json"
jq --arg path "${attacker_bin}" '.installation.path = $path' \
  "${local_receipt}" >"${caller_receipt_stage}"
caller_receipt_sha="$(sha256sum "${caller_receipt_stage}")"
caller_receipt_sha="${caller_receipt_sha%% *}"
caller_receipt="${tmp}/${caller_receipt_sha}.json"
mv -- "${caller_receipt_stage}" "${caller_receipt}"

# Exercise the exact production logic without requiring write access beneath
# /opt: only this automatically removed test copy substitutes the fixed broker
# path, while every other byte remains the reviewed source.
test_lib="${tmp}/lib.sh"
sed "s#${production_broker}#${broker_bin}#g" "${source_lib}" >"${test_lib}"
test_verifier="${tmp}/ensure-jankurai.sh"
sed "s#${production_broker}#${broker_bin}#g" "${source_verifier}" >"${test_verifier}"
test_local_lib="${tmp}/local-lib.sh"
sed -e "s#${production_broker}#${broker_bin}#g" \
  -e "s#/home/ubuntu/.jeryu#${tmp}/home/.jeryu#g" \
  "${source_lib}" >"${test_local_lib}"

run_release_broker() {
  local path="$1"
  local release_command
  shift
  # The child shell expands its positional inputs.
  # shellcheck disable=SC2016
  release_command='source "$1"; require_jankurai; [[ "$JERYU_GOVERNED_JANKURAI_BIN" == "$2" ]]'
  env -i HOME="${tmp}/home" PATH="${path}:/usr/bin:/bin" \
    JAIN_RELEASE_CI=1 "$@" bash -c \
    "${release_command}" \
    bash "${test_lib}" "${broker_bin}"
}

run_release_broker "${tmp}/broker/bin"
env -i HOME="${tmp}/home" PATH="${tmp}/broker/bin:/usr/bin:/bin" \
  JAIN_RELEASE_CI=1 bash "${test_verifier}" >/dev/null

# Ordinary local mode keeps the governed home installation authoritative even
# when an older same-version binary appears earlier on the real default PATH.
# shellcheck disable=SC2016
env -i HOME="${tmp}/home" PATH="${tmp}/home/.local/bin:/usr/bin:/bin" \
  bash -c 'source "$1"; require_jankurai; [[ "$JERYU_GOVERNED_JANKURAI_BIN" == "$2" ]]; [[ "$(command -v jankurai)" == jankurai ]]; [[ "$(jankurai --version)" == "$JERYU_JANKURAI_VERSION" ]]' \
  bash "${test_local_lib}" "${ambient_bin}"
# shellcheck disable=SC2016
env -i HOME="${tmp}/home" PATH="/usr/bin:/bin" \
  JERYU_GOVERNED_JANKURAI_BIN="${attacker_bin}" \
  JERYU_JANKURAI_RECEIPT="${caller_receipt}" \
  bash -c 'source "$1"; require_jankurai; [[ "$JERYU_GOVERNED_JANKURAI_BIN" == "$2" ]]' \
  bash "${test_local_lib}" "${attacker_bin}"

# Caller substitutions never select the auditor: the broker-controlled PATH and
# fixed authority path remain decisive.
run_release_broker "${tmp}/broker/bin" \
  JERYU_GOVERNED_JANKURAI_BIN="${attacker_bin}" \
  JERYU_JANKURAI_BIN="${attacker_bin}"
expect_failure "caller receipt substitution" \
  "release broker Jankurai rejects caller receipt authority" \
  run_release_broker "${tmp}/broker/bin" \
  JERYU_JANKURAI_RECEIPT="${tmp}/caller-receipt.json" \
  JERYU_JANKURAI_RECEIPT_SHA256="$(printf 'a%.0s' {1..64})" \
  JERYU_JANKURAI_ALLOW_TEST_RECEIPT=1

expect_failure "ambient home auditor" "release broker Jankurai path mismatch" \
  run_release_broker "${tmp}/home/.jeryu/bin"
expect_failure "caller PATH substitution" "release broker Jankurai path mismatch" \
  run_release_broker "${tmp}/attacker/bin"
expect_failure "missing broker auditor" "release broker Jankurai path mismatch" \
  run_release_broker "/usr/bin:/bin"

cp -- "${broker_bin}" "${tmp}/governed-backup"
chmod 0755 "${broker_bin}"
printf '#!/usr/bin/env bash\nprintf "%s\\n"\n' "${expected_version}" >"${broker_bin}"
chmod 0555 "${broker_bin}"
expect_failure "wrong broker binary" "the host has not installed the current pin" \
  run_release_broker "${tmp}/broker/bin"
chmod 0755 "${broker_bin}"
rm -f -- "${broker_bin}"
mv -- "${tmp}/governed-backup" "${broker_bin}"
chmod 0555 "${broker_bin}"

chmod 0755 "${broker_bin}"
expect_failure "writable broker binary" "release broker Jankurai custody mismatch" \
  run_release_broker "${tmp}/broker/bin"
chmod 0555 "${broker_bin}"

ln "${broker_bin}" "${tmp}/broker/bin/jankurai-linked"
expect_failure "linked broker binary" "release broker Jankurai custody mismatch" \
  run_release_broker "${tmp}/broker/bin"
rm -f -- "${tmp}/broker/bin/jankurai-linked"

# Freshness: the ordinary home installation must be the one the host authority names,
# and a release broker must carry its read-only digest record.
stamp="${tmp}/home/.jeryu/authority/jankurai.json"
cp -- "${stamp}" "${tmp}/stamp-backup"
jq --arg sha "$(printf 'f%.0s' {1..64})" '.binary_sha256 = $sha' "${tmp}/stamp-backup" >"${stamp}"
# shellcheck disable=SC2016
expect_failure "stale host authority" "the host has not installed the current pin" \
  env -i HOME="${tmp}/home" PATH="/usr/bin:/bin" \
  bash -c 'source "$1"; require_jankurai' bash "${test_local_lib}"
mv -- "${tmp}/stamp-backup" "${stamp}"
chmod 0644 "${broker_bin}.sha256"
expect_failure "writable broker digest record" "release broker Jankurai digest record custody mismatch" \
  run_release_broker "${tmp}/broker/bin"
chmod 0444 "${broker_bin}.sha256"

printf 'governed Jankurai path tests passed: broker ambient env path receipt missing identity custody freshness\n'
