#!/bin/sh
set -eu

theseus report recorded-exploration
grep -Fq 'Timeline tree' recorded-exploration/theseus-report/index.html
grep -Fq 'Dirty-page footprint' recorded-exploration/theseus-report/index.html
theseus report --format markdown --output failure.md recorded-exploration
grep -Fq 'Timeline recipes' failure.md
theseus report --format junit --output theseus-results.xml recorded-exploration
grep -Fq '<testsuite' theseus-results.xml
echo 'PASS: Theseus wrote browser, issue, and CI reports from one locked exploration'
