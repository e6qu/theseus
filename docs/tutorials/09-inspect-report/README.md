# Tutorial 9: Inspect a recorded exploration

Render browser, Markdown, and JUnit reports from the recorded bundle in this
directory. No KVM, Docker, guest build, or Theseus checkout is needed.

Run every command from this directory with a published `theseus` binary on
`PATH`.

## 1. Render the browser report

```sh
theseus report recorded-exploration
grep -F 'Timeline tree' recorded-exploration/theseus-report/index.html
grep -F 'Dirty-page footprint' recorded-exploration/theseus-report/index.html
```

Open `recorded-exploration/theseus-report/index.html`. The dirty-page count is
a checkpoint footprint, not source-code coverage.

## 2. Render issue and CI formats

```sh
theseus report --format markdown --output failure.md recorded-exploration
grep -F 'Timeline recipes' failure.md
theseus report --format junit --output theseus-results.xml recorded-exploration
grep -F '<testsuite' theseus-results.xml
```

Keep the recorded exploration beside these reports. The reports explain the
result; the bundle contains its replay inputs.

## 3. Clean up (optional)

```sh
rm -rf recorded-exploration/theseus-report failure.md theseus-results.xml
```
