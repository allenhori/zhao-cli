---
name: Feature request
about: Something zhao doesn't do yet, that you think it should
title: ''
labels: enhancement
assignees: ''
---

## The problem

What you're actually trying to do, and where zhao falls short of it right now -- not the
solution yet, just the gap. ("I want to know X" or "zhao doesn't detect Y" is more useful here
than "add a --flag that does Z.")

## A concrete example

A real (or realistic) case where this would have mattered -- a specific column change, a
specific project shape, a specific command you wish existed. Concrete beats abstract; it's
what turns "would be nice" into something actually buildable.

## What you'd want zhao to do instead

If you have a shape in mind for the fix (a new flag, a new Rule, different output), sketch it
-- but the problem above matters more than getting this part exactly right.

## Is this in scope for zhao-core, or specific to your setup?

zhao-core is deliberately format-agnostic (no dbt-specific vocabulary baked in -- see
[ARCHITECTURE.md](../../ARCHITECTURE.md)), and zhao runs fully offline with no warehouse
connection beyond what `--check-relations` optionally needs (see the README's "What it doesn't
do"). If your request needs either of those to change, say so explicitly -- it changes how
big a decision this is, not whether it's worth raising.
