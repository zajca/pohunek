---
kind: added
---
A named pohunek session now passes its name to Pi (`pi --name <name>`) at launch, resume and fork, so Pi shows it in its footer and terminal title; the Pi package 1.1.0 needs pohunek 0.39. A runtime descriptor can now declare the name as a separate argument (`launch_args = ["--name", "{session_name}"]`); a name that starts with `-` is then not passed, so it can never be read as an option.
