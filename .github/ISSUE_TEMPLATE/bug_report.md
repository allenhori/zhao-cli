---
name: Bug report
about: Something zhao does is wrong -- a wrong result, a crash, an unclear/incorrect error
title: ''
labels: bug
assignees: ''
---

## What happened

What you ran, and what zhao actually did.

## What you expected instead

What zhao should have done -- the correct output, the correct exit code, whichever.

## A minimal repro, if you can manage one

The single most useful thing you can attach: a small dbt project (or just the relevant
`target/manifest.json`) that reproduces this on its own, plus the exact `zhao` command and
flags you ran. If the real project can't be shared, a stripped-down version showing the same
shape (same column names/types, same join, whatever's actually relevant) is far more useful
than a description of it.

## Version

```
zhao --version
```

Also worth including: your platform (macOS/Linux/Windows), and whether you installed via the
install script, `cargo install`, or built from source.

## Anything else that might matter

`target/zhao/run-metadata.json` or `target/zhao/logs/<date>.log` from the run in question, if
either exists and looks relevant -- both are already machine-readable and never contain
credentials or row data (see the README's "What it doesn't do").
