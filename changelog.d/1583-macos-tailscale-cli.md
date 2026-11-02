---
kind: added
---
On Apple Silicon Macs, the Tailscale overlay now finds the `tailscale` CLI that `brew install tailscale` puts in `/opt/homebrew/bin`. Start `tailscaled` yourself and join the tailnet; Pohunek does not start it. The App Store and standalone builds are still refused. A macOS daemon listening on the tailnet address is not verified yet.
