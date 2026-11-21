---
kind: changed
---
The agent profiles page describes how a launch option over a wrapper program picks a credential while profiles share one config home, and warns that a resume re-reads whatever the wrapper reads. `config_home_id` now reads as "same directory", not "same account". The Claude package guide has a section for running one session per Claude subscription with a `claude setup-token` token, with the credential precedence, the bare-mode and Remote Control limits, and the rule that the account follows whose work it is, never remaining quota. Automatic switching on limits is not supported. ([#1562](https://github.com/zajca/pohunek/issues/1562))
