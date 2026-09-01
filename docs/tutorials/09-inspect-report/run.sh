#!/bin/sh
set -eu

theseus report recorded-exploration
grep -Fq 'Timeline tree' recorded-exploration/theseus-report/index.html
grep -Fq 'Dirty-page footprint' recorded-exploration/theseus-report/index.html
echo 'PASS: Theseus wrote a self-contained exploration report'
