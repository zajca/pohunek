---
kind: fixed
---
A launch no longer waits indefinitely on the trusted-checkout check when a file system call below a declared path stalls (for example a hung mount): after the 30 second deadline the checkout is treated as not trusted, the trusted arguments are withheld, and one `checkout_trust.failed` warning is logged. While that check is still blocked, another launch in the same directory is withheld at once.
