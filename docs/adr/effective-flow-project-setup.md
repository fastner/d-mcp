# Effective Flow project setup

## Status

Active

## Context

This ADR holds this project's tracked Effective Flow configuration. `.effective-flow/` is a pure
runtime directory and completely gitignored.

## Configuration

| Key                               | Value                      |
| --------------------------------- | -------------------------- |
| review.profile                    | focused                    |
| review.autoConfirmScope           | false                      |
| review.designDecisionSources      | standard                   |
| review.validation                 | full                       |
| applyReview.defaultCommitStrategy | null                       |
| applyReview.finalValidation       | full                       |
| applyReview.stashPolicy           | interactive                |
| applyReview.worktree.baseDir      | .effective-flow/.worktrees |
| applyReview.worktree.setup        | auto                       |
| worktree.enabled                  | true                       |
| delivery.completion               | pr                         |
| delivery.baseBranch               | origin/main                |
| tracker.mode                      | remote                     |
| tracker.remoteToolOverride        | auto                       |
| plan.dir                          | docs/plan                  |
| language.project                  | en                         |
| language.chat                     | de                         |
