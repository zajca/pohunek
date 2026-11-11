---
kind: security
---
The trusted-checkout check now opens the launch directory once and opens every directory and file below it relative to its parent without following a symbolic link, so a declared directory replaced by a link while the check runs makes the checkout untrusted instead of being read through the link. A declared file is read in-process up to its 4 MiB bound, so a file that grows after the size check is refused at once and no longer runs a git command to its timeout with a `checkout_trust.failed` warning; all declared files together are limited to 64 MiB. The contents of all declared files are hashed by one git process instead of one per file.
