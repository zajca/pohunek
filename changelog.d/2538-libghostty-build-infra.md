---
kind: changed
---
Release builds now provision the pinned libghostty-vt 1.0.2 static library for the glibc, MUSL and Apple Silicon targets with a pinned Zig 0.17.0, and the release evidence attests its pin (library and tarball sha256, Zig version). This is build infrastructure for the libghostty screen model planned for 0.46; no shipped binary links the library yet and no behavior changes. ([#2538](https://github.com/zajca/pohunek/issues/2538))
