---
kind: fixed
---
`pohunek setup` keeps a serving assistant selection that is not older than the listed one (`present`), even when the listed digest is installed but not selected, disabled or revoked. A run whose newer selection serves used to end as a gap asking the owner to select the listed digest by hand; it now completes and leaves the selection alone. A selection that no longer serves, or that is older than the listing, is still replaced only when the listed digest is not installed: an installed listed digest the owner left disabled or unselected stays a gap naming the `plugin enable` and `plugin select` commands, and the apply still fails `setup_incomplete` there.
