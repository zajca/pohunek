---
kind: added
---
On Apple Silicon Macs with a stock Homebrew prefix, Pohunek now finds the `tailscale` CLI from `brew install tailscale` in `/opt/homebrew/bin`, so `pohunek doctor` and `host list` report the Tailscale state instead of a missing CLI. Pohunek does not start `tailscaled`. No part of a running `tailscaled` is verified on macOS, neither the client side nor the listener on the tailnet address. The App Store and standalone builds are still refused.
