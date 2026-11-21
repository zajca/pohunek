---
kind: fixed
---
A daemon that follows the systemd user manager's `PATH` now runs `git` and the worktree hooks on the `PATH` of the session registry that started the operation. `host.inspect` reports `git_available`, and a `post-create` or other worktree hook sees its `PATH`, from that registry's own read of the manager, instead of the read another registry in the same process made last. One `session.new` takes the `PATH` once and uses it for the agent probe, the worktree bind and its hooks, the checkout-trust check, the `SessionStart` hook and the launch; `session.resume` does the same for its probe, checkout-trust check and launch. ([#2494](https://github.com/zajca/pohunek/issues/2494))
