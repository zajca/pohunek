---
kind: added
---
A runtime package can pass the pohunek session name to its agent at launch, resume and fork with `[naming] launch_args` (runtime feature `naming-launch-args-v1`), either joined into one `-<flag>=` token (`--name={session_name}`) or as a separate element after a flag (`["--name", "{session_name}"]`). In the separate form a name that starts with `-` is not passed at all, so an agent can never read it as an option. The official Claude and Pi packages declare it.
