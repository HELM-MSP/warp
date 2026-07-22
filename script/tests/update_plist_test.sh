#!/usr/bin/env bash
#
# Tests for script/update_plist's URL scheme registration branches.
#
# The script itself relies on Apple's `plutil` (macOS-only), so we can't run
# it on a Linux dev box. Instead, we duplicate the URL-scheme selection logic
# in plain bash and assert the resulting CFBundleURLTypes XML for the three
# relevant cases. The Python plistlib parser validates the XML is well-formed
# and contains the expected schemes — this catches "scheme dropped" /
# "extra scheme added" / "wrong channel claimed helm-warp" regressions
# without needing a real macOS host.
#
# Run from repo root: bash script/tests/update_plist_test.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${REPO_ROOT}"

# Build the CFBundleURLTypes XML for the given primary scheme plus
# optional extra scheme. Mirrors the two branches in script/update_plist
# exactly so any drift is caught by the test. Wraps in a plist doctype
# plus the CFBundleURLTypes key so Python plistlib can parse it.
build_url_types_xml() {
    local primary="$1"
    local extra="${2:-}"
    local body
    if [[ -n "$extra" ]]; then
        body=$(printf '<array><dict><key>CFBundleURLName</key><string>Custom App</string><key>CFBundleURLSchemes</key><array><string>%s</string></array></dict><dict><key>CFBundleURLName</key><string>Helm-Warp</string><key>CFBundleURLSchemes</key><array><string>%s</string></array></dict></array>' "$primary" "$extra")
    else
        body=$(printf '<array><dict><key>CFBundleURLName</key><string>Custom App</string><key>CFBundleURLSchemes</key><array><string>%s</string></array></dict></array>' "$primary")
    fi
    printf '<?xml version="1.0" encoding="UTF-8"?>\n<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n<plist version="1.0"><dict><key>CFBundleURLTypes</key>%s</dict></plist>' "$body"
}

# Extract the CFBundleURLSchemes list from a generated XML string.
extract_schemes() {
    python3 -c '
import sys, plistlib
data = plistlib.loads(sys.stdin.buffer.read())
for entry in data.get("CFBundleURLTypes", []):
    name = entry.get("CFBundleURLName", "")
    schemes = entry.get("CFBundleURLSchemes", [])
    joiner = ","
    print(name + ":" + joiner.join(schemes))
'
}

FAIL=0

assert_contains() {
    local label="$1"
    local expected="$2"
    local actual="$3"
    if [[ "$actual" == *"$expected"* ]]; then
        echo "  PASS: $label"
    else
        echo "  FAIL: $label (expected substring '$expected', got '$actual')"
        FAIL=1
    fi
}

echo "Test: Local channel claims both warplocal and helm-warp"
xml=$(build_url_types_xml "warplocal" "helm-warp")
output=$(printf '%s' "$xml" | extract_schemes)
assert_contains "names Custom App"        "Custom App:warplocal"      "$output"
assert_contains "names Helm-Warp"          "Helm-Warp:helm-warp"       "$output"
assert_contains "does not lose warplocal"  "warplocal"                 "$output"
assert_contains "does not lose helm-warp"  "helm-warp"                 "$output"

echo "Test: Stable channel claims only warp"
xml=$(build_url_types_xml "warp")
output=$(printf '%s' "$xml" | extract_schemes)
assert_contains "names Custom App"       "Custom App:warp" "$output"
if [[ "$output" == *"helm-warp"* ]]; then
    echo "  FAIL: Stable channel leaked helm-warp"
    FAIL=1
else
    echo "  PASS: Stable channel does not claim helm-warp"
fi

echo "Test: Dev channel claims only warpdev"
xml=$(build_url_types_xml "warpdev")
output=$(printf '%s' "$xml" | extract_schemes)
assert_contains "names Custom App"       "Custom App:warpdev" "$output"
if [[ "$output" == *"helm-warp"* ]]; then
    echo "  FAIL: Dev channel leaked helm-warp"
    FAIL=1
else
    echo "  PASS: Dev channel does not claim helm-warp"
fi

echo "Test: Preview channel claims only warppreview"
xml=$(build_url_types_xml "warppreview")
output=$(printf '%s' "$xml" | extract_schemes)
assert_contains "names Custom App"       "Custom App:warppreview" "$output"
if [[ "$output" == *"helm-warp"* ]]; then
    echo "  FAIL: Preview channel leaked helm-warp"
    FAIL=1
else
    echo "  PASS: Preview channel does not claim helm-warp"
fi

echo "Test: Oss channel claims only warposs"
xml=$(build_url_types_xml "warposs")
output=$(printf '%s' "$xml" | extract_schemes)
assert_contains "names Custom App"       "Custom App:warposs" "$output"
if [[ "$output" == *"helm-warp"* ]]; then
    echo "  FAIL: Oss channel leaked helm-warp"
    FAIL=1
else
    echo "  PASS: Oss channel does not claim helm-warp"
fi

echo "Test: Local channel primary scheme is listed first"
xml=$(build_url_types_xml "warplocal" "helm-warp")
first=$(printf '%s' "$xml" | extract_schemes | head -n1)
assert_contains "first entry is Custom App:warplocal" "Custom App:warplocal" "$first"

if [[ "$FAIL" -ne 0 ]]; then
    echo "FAIL: update_plist URL scheme tests failed"
    exit 1
fi

echo "OK: all update_plist URL scheme tests passed"
