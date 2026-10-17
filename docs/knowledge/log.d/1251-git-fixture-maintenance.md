## Unreleased (2026-10-17, script tests no longer race git's background maintenance, #1251)

- The repository's script tests create their throwaway git repositories with
  git's automatic maintenance turned off. The background maintenance that
  `git commit` starts could otherwise still be writing into `.git` while a
  test removed the repository, which failed the test intermittently with
  "Directory not empty".
