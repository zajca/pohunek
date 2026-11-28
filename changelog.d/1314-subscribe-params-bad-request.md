---
kind: changed
---
`subscribe` parameters are parsed now: a request whose params are neither an object nor `null` fails with `daemon/bad_request`, where a daemon that predates the parse ignored them. An unknown key inside the object is still ignored. ([#1314](https://github.com/zajca/pohunek/issues/1314))
